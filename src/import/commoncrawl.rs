//! Import aus Common Crawl (https://commoncrawl.org): ein frei nutzbares
//! Web-Archiv mit mehreren Milliarden Seiten pro monatlichem Crawl.
//!
//! Jeder Crawl besteht aus ~90 000 WARC-Dateien à ~1 GB (≈30 000 Seiten).
//! Wayon lädt sie nacheinander per Stream, extrahiert die HTML-Seiten parallel
//! und merkt sich erledigte Dateien in `crawl/cc-done.txt` – ein erneuter
//! Aufruf macht mit den nächsten Dateien weiter.

use super::warc::{warc_date_secs, HttpResponse, WarcReader};
use super::{fetch_text, open_source, BatchWriter, DoneList, ImportStats, LangFilter};
use crate::crawler::record_from_html;
use crate::store::{PageRecord, StoreWriter};
use crate::urlnorm;
use anyhow::{Context, Result};
use std::io::{BufReader, Read};
use std::path::Path;
use url::Url;

pub const DATA_BASE: &str = "https://data.commoncrawl.org/";
pub const INDEX_INFO: &str = "https://index.commoncrawl.org/collinfo.json";

/// Kennung des neuesten Crawls, z.B. `CC-MAIN-2026-44`.
pub fn latest_crawl() -> Result<String> {
    let txt = fetch_text(INDEX_INFO)?;
    let v: serde_json::Value = serde_json::from_str(&txt).context("collinfo.json unlesbar")?;
    v.as_array()
        .and_then(|a| a.first())
        .and_then(|c| c["id"].as_str())
        .map(str::to_string)
        .context("keine Crawls in collinfo.json")
}

/// Pfade aller WARC-Dateien eines Crawls.
pub fn warc_paths(crawl: &str) -> Result<Vec<String>> {
    let url = format!("{DATA_BASE}crawl-data/{crawl}/warc.paths.gz");
    let mut s = String::new();
    flate2::read::MultiGzDecoder::new(open_source(&url)?).read_to_string(&mut s)?;
    Ok(s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

fn convert(resp: HttpResponse) -> Option<PageRecord> {
    if resp.status != 200 {
        return None;
    }
    if let Some(ct) = &resp.content_type {
        let l = ct.to_ascii_lowercase();
        if !(l.contains("text/html") || l.contains("xhtml")) {
            return None;
        }
    }
    let u = urlnorm::normalize(&Url::parse(&resp.url).ok()?)?;
    if !urlnorm::looks_crawlable(&u) {
        return None;
    }
    Some(record_from_html(
        &u,
        &resp.body,
        resp.content_type.as_deref(),
        &resp.robots_tag,
        warc_date_secs(&resp.date),
    ))
}

/// Liest einen (gzip-komprimierten) WARC-Strom; die ersten `skip` Antworten
/// werden übersprungen (Fortsetzen nach Abbruch). Liefert die Anzahl gelesener Antworten.
fn import_stream(
    src: Box<dyn Read + Send>,
    bw: &mut BatchWriter<HttpResponse>,
    skip: u64,
    read: &mut u64,
) -> Result<()> {
    let gz = flate2::read::MultiGzDecoder::new(src);
    let mut r = WarcReader::new(BufReader::with_capacity(1 << 20, gz));
    while let Some(resp) = r.next_response()? {
        *read += 1;
        if *read <= skip {
            continue;
        }
        let size = resp.body.len();
        bw.push(resp, size, &convert)?;
    }
    Ok(())
}

/// Importiert eine WARC-Datei (lokaler Pfad oder URL). Bricht der Download ab,
/// wird bis zu dreimal neu begonnen und bereits verarbeitete Antworten übersprungen.
fn import_one(src: &str, bw: &mut BatchWriter<HttpResponse>) -> Result<()> {
    let mut handled = 0u64;
    let mut attempt = 0;
    loop {
        let mut read = 0u64;
        let res = open_source(src).and_then(|s| import_stream(s, bw, handled, &mut read));
        bw.flush(&convert)?;
        match res {
            Ok(()) => return Ok(()),
            Err(e) if attempt < 3 && src.starts_with("http") => {
                attempt += 1;
                handled = handled.max(read);
                log::warn!("{src}: {e:#} – Versuch {attempt}/3 in {}s", 5 * attempt);
                std::thread::sleep(std::time::Duration::from_secs(5 * attempt));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Importiert lokale oder entfernte WARC-Dateien.
pub fn import_warc_files(
    files: &[String],
    store_dir: &Path,
    filter: LangFilter,
) -> Result<ImportStats> {
    let mut writer = StoreWriter::open(store_dir)?;
    let mut bw = BatchWriter::new(&mut writer, filter);
    for f in files {
        log::info!("Importiere {f}");
        import_one(f, &mut bw)?;
    }
    let stats = bw.stats.clone();
    writer.flush()?;
    Ok(stats)
}

/// Importiert die nächsten `files` noch nicht verarbeiteten WARC-Dateien eines Crawls.
pub fn import_cc(
    store_dir: &Path,
    crawl: Option<String>,
    files: usize,
    filter: LangFilter,
) -> Result<ImportStats> {
    std::fs::create_dir_all(store_dir)?;
    let crawl = match crawl {
        Some(c) => c,
        None => {
            latest_crawl().context("neuesten Common-Crawl nicht ermittelbar (--crawl angeben)")?
        }
    };
    let paths = warc_paths(&crawl)?;
    let mut done = DoneList::open(&store_dir.join("cc-done.txt"))?;
    log::info!(
        "Common Crawl {crawl}: {} WARC-Dateien, {} bereits importiert, importiere {files} weitere",
        paths.len(),
        done.len()
    );
    let mut writer = StoreWriter::open(store_dir)?;
    let mut bw = BatchWriter::new(&mut writer, filter);
    let mut n = 0;
    for (i, path) in paths.iter().enumerate() {
        if n >= files {
            break;
        }
        if done.contains(path) {
            continue;
        }
        let before = bw.stats.stored;
        log::info!(
            "[{}/{files}] {path} (Datei {} von {})",
            n + 1,
            i + 1,
            paths.len()
        );
        match import_one(&format!("{DATA_BASE}{path}"), &mut bw) {
            Ok(()) => {
                done.mark(path)?;
                log::info!("  {} Seiten übernommen", bw.stats.stored - before);
            }
            Err(e) => log::error!("  übersprungen: {e:#}"),
        }
        n += 1;
    }
    let stats = bw.stats.clone();
    writer.flush()?;
    Ok(stats)
}
