//! Import der Wikimedia-Enterprise-HTML-Dumps
//! (https://dumps.wikimedia.org/other/enterprise_html/): alle Artikel einer
//! Wikipedia als HTML, ein JSON-Objekt pro Zeile in einem `.tar.gz`.
//!
//! Die Artikel verlinken sich gegenseitig (`./Titel`), dadurch entstehen beim
//! Indexieren PageRank und Ankertexte genau wie bei gecrawlten Seiten.

use super::warc::warc_date_secs;
use super::{fetch_text, open_source, BatchWriter, ImportStats, LangFilter};
use crate::crawler::record_from_html;
use crate::store::{PageRecord, StoreWriter};
use crate::text::Lang;
use crate::urlnorm;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::io::{BufRead, BufReader};
use std::path::Path;
use url::Url;

pub const RUNS_URL: &str = "https://dumps.wikimedia.org/other/enterprise_html/runs/";

#[derive(Deserialize)]
struct Article {
    name: Option<String>,
    url: Option<String>,
    #[serde(rename = "abstract")]
    summary: Option<String>,
    date_modified: Option<String>,
    article_body: Option<Body>,
    in_language: Option<Ident>,
    namespace: Option<NsIdent>,
}

#[derive(Deserialize)]
struct Body {
    html: Option<String>,
}

#[derive(Deserialize)]
struct Ident {
    identifier: Option<String>,
}

#[derive(Deserialize)]
struct NsIdent {
    identifier: Option<i64>,
}

/// URL des neuesten vollständigen Artikel-Dumps einer Sprache (z.B. "de").
pub fn latest_dump_url(lang: &str) -> Result<String> {
    let listing = fetch_text(RUNS_URL)?;
    let mut dates: Vec<String> = listing
        .split("href=\"")
        .skip(1)
        .filter_map(|s| s.split('"').next())
        .map(|s| s.trim_end_matches('/').to_string())
        .filter(|s| s.len() == 8 && s.chars().all(|c| c.is_ascii_digit()))
        .collect();
    dates.sort();
    dates.dedup();
    let client = super::http_client()?;
    // Der neueste Lauf kann noch unvollständig sein → bis zu drei Läufe zurück prüfen
    for d in dates.iter().rev().take(3) {
        let url = format!("{RUNS_URL}{d}/{lang}wiki-NS0-{d}-ENTERPRISE-HTML.json.tar.gz");
        if client
            .head(&url)
            .send()
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            return Ok(url);
        }
    }
    anyhow::bail!("kein Dump für '{lang}' unter {RUNS_URL} gefunden")
}

fn convert(line: String) -> Option<PageRecord> {
    let a: Article = serde_json::from_str(&line).ok()?;
    if a.namespace.and_then(|n| n.identifier).unwrap_or(0) != 0 {
        return None;
    }
    let html = a.article_body?.html?;
    let u = urlnorm::normalize(&Url::parse(a.url.as_deref()?).ok()?)?;
    let fetched = a
        .date_modified
        .as_deref()
        .map(warc_date_secs)
        .unwrap_or_else(crate::crawler::now_secs);
    let mut rec = record_from_html(
        &u,
        html.as_bytes(),
        Some("text/html; charset=utf-8"),
        "",
        fetched,
    );
    if let Some(name) = a.name.filter(|n| !n.is_empty()) {
        rec.title = name;
    }
    if rec.description.is_empty() {
        if let Some(s) = a.summary {
            rec.description = crate::crawler::prefix(s.trim(), 1000).to_string();
        }
    }
    if let Some(code) = a.in_language.and_then(|l| l.identifier) {
        let l = Lang::from_code(&code);
        if l != Lang::Other {
            rec.lang = l as u8;
        }
    }
    Some(rec)
}

/// Importiert einen Dump (lokale `.tar.gz`-Datei oder URL).
pub fn import_wikipedia(src: &str, store_dir: &Path) -> Result<ImportStats> {
    let mut writer = StoreWriter::open(store_dir)?;
    let mut bw = BatchWriter::new(&mut writer, LangFilter::parse("all"));
    let gz = flate2::read::MultiGzDecoder::new(open_source(src)?);
    let mut archive = tar::Archive::new(gz);
    for entry in archive.entries().context("kein gültiges tar-Archiv")? {
        let entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();
        if !(path.ends_with(".ndjson") || path.ends_with(".json")) {
            continue;
        }
        log::info!("Lese {path}");
        let mut r = BufReader::with_capacity(4 << 20, entry);
        let mut line = String::new();
        loop {
            line.clear();
            if r.read_line(&mut line)? == 0 {
                break;
            }
            if line.trim().is_empty() {
                continue;
            }
            let size = line.len();
            bw.push(std::mem::take(&mut line), size, &convert)?;
        }
    }
    bw.flush(&convert)?;
    let stats = bw.stats.clone();
    writer.flush()?;
    Ok(stats)
}
