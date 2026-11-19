//! Importer für große, frei verfügbare Datensätze – der schnellste Weg zu einem
//! großen Index, ohne jede Seite selbst crawlen zu müssen:
//!
//! * [`commoncrawl`] – Common Crawl: monatlich mehrere Milliarden Webseiten als
//!   WARC-Dateien (je ~1 GB, ~30 000 Seiten).
//! * [`wikipedia`] – Wikimedia-HTML-Dumps: alle Artikel einer Sprache inkl.
//!   interner Verlinkung (für PageRank und Ankertexte).
//!
//! Beide schreiben in denselben Crawl-Speicher wie der Crawler; danach genügt
//! `wayon index`.

pub mod commoncrawl;
pub mod warc;
pub mod wikipedia;

use crate::store::{PageRecord, StoreWriter};
use crate::text::Lang;
use anyhow::{Context, Result};
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

/// Sprachfilter für Importe.
#[derive(Clone, Debug, Default)]
pub struct LangFilter {
    langs: Vec<Lang>,
    keep_other: bool,
}

impl LangFilter {
    /// `"de,en"`, `"de"`, `"all"` …
    pub fn parse(spec: &str) -> LangFilter {
        let mut f = LangFilter::default();
        for part in spec.split(',').map(|s| s.trim().to_ascii_lowercase()) {
            match part.as_str() {
                "" => {}
                "all" | "*" => f.keep_other = true,
                "de" => f.langs.push(Lang::De),
                "en" => f.langs.push(Lang::En),
                _ => f.keep_other = true,
            }
        }
        f
    }

    pub fn accepts(&self, lang: u8) -> bool {
        if self.keep_other {
            return true;
        }
        self.langs.is_empty() || self.langs.contains(&Lang::from_u8(lang))
    }
}

/// Zähler für den Fortschritt eines Imports.
#[derive(Debug, Default, Clone)]
pub struct ImportStats {
    pub seen: u64,
    pub stored: u64,
    pub filtered: u64,
    pub bytes_in: u64,
}

/// Öffnet eine lokale Datei oder eine http(s)-URL als blockierenden Datenstrom.
pub fn open_source(src: &str) -> Result<Box<dyn Read + Send>> {
    if src.starts_with("http://") || src.starts_with("https://") {
        let resp = http_client()?
            .get(src)
            .send()
            .with_context(|| format!("Download fehlgeschlagen: {src}"))?;
        let status = resp.status();
        anyhow::ensure!(status.is_success(), "HTTP {status} für {src}");
        Ok(Box::new(resp))
    } else {
        let f = std::fs::File::open(src).with_context(|| format!("{src} nicht lesbar"))?;
        Ok(Box::new(f))
    }
}

pub fn http_client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent(format!(
            "WayonBot/{} (+https://github.com/hackyannick/wayon)",
            env!("CARGO_PKG_VERSION")
        ))
        .connect_timeout(Duration::from_secs(20))
        // Große Dateien (Wikipedia-Dumps: zig GB): großzügiges Gesamt-Timeout,
        // tote Verbindungen erkennt TCP-Keepalive
        .timeout(Duration::from_secs(12 * 3600))
        .tcp_keepalive(Duration::from_secs(30))
        .build()?)
}

/// Lädt eine kleine Textdatei (z.B. Pfadlisten) vollständig.
pub fn fetch_text(url: &str) -> Result<String> {
    let mut s = String::new();
    open_source(url)?.read_to_string(&mut s)?;
    Ok(s)
}

/// Sammelt Rohdokumente, wandelt sie parallel in Records um und schreibt sie.
pub struct BatchWriter<'a, T: Send> {
    writer: &'a mut StoreWriter,
    filter: LangFilter,
    batch: Vec<T>,
    batch_bytes: usize,
    pub stats: ImportStats,
    started: Instant,
    last_log: Instant,
}

const BATCH_DOCS: usize = 256;
const BATCH_BYTES: usize = 64 << 20;

impl<'a, T: Send> BatchWriter<'a, T> {
    pub fn new(writer: &'a mut StoreWriter, filter: LangFilter) -> Self {
        BatchWriter {
            writer,
            filter,
            batch: Vec::with_capacity(BATCH_DOCS),
            batch_bytes: 0,
            stats: ImportStats::default(),
            started: Instant::now(),
            last_log: Instant::now(),
        }
    }

    /// Fügt ein Rohdokument hinzu; `convert` läuft später parallel.
    pub fn push<F>(&mut self, item: T, size: usize, convert: &F) -> Result<()>
    where
        F: Fn(T) -> Option<PageRecord> + Sync,
    {
        self.stats.seen += 1;
        self.stats.bytes_in += size as u64;
        self.batch.push(item);
        self.batch_bytes += size;
        if self.batch.len() >= BATCH_DOCS || self.batch_bytes >= BATCH_BYTES {
            self.flush(convert)?;
        }
        Ok(())
    }

    pub fn flush<F>(&mut self, convert: &F) -> Result<()>
    where
        F: Fn(T) -> Option<PageRecord> + Sync,
    {
        use rayon::prelude::*;
        let items = std::mem::take(&mut self.batch);
        self.batch_bytes = 0;
        let records: Vec<Option<PageRecord>> = items.into_par_iter().map(convert).collect();
        for r in records {
            match r {
                Some(r) if self.filter.accepts(r.lang) && r.body.len() >= 100 => {
                    self.writer.append(&r)?;
                    self.stats.stored += 1;
                }
                _ => self.stats.filtered += 1,
            }
        }
        if self.last_log.elapsed() >= Duration::from_secs(10) {
            self.last_log = Instant::now();
            let secs = self.started.elapsed().as_secs_f64().max(0.001);
            log::info!(
                "{} gelesen, {} gespeichert, {} gefiltert | {:.0} Dokumente/s, {:.1} MiB/s",
                self.stats.seen,
                self.stats.stored,
                self.stats.filtered,
                self.stats.seen as f64 / secs,
                self.stats.bytes_in as f64 / secs / 1048576.0
            );
        }
        Ok(())
    }
}

/// Liste erledigter Dateien (für fortsetzbare Importe).
pub struct DoneList {
    path: std::path::PathBuf,
    done: std::collections::HashSet<String>,
}

impl DoneList {
    pub fn open(path: &Path) -> Result<DoneList> {
        let done = match std::fs::read_to_string(path) {
            Ok(s) => s.lines().map(str::to_string).collect(),
            Err(_) => Default::default(),
        };
        Ok(DoneList {
            path: path.to_path_buf(),
            done,
        })
    }

    pub fn contains(&self, item: &str) -> bool {
        self.done.contains(item)
    }

    pub fn len(&self) -> usize {
        self.done.len()
    }

    pub fn is_empty(&self) -> bool {
        self.done.is_empty()
    }

    pub fn mark(&mut self, item: &str) -> Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(f, "{item}")?;
        self.done.insert(item.to_string());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_filter() {
        let f = LangFilter::parse("de,en");
        assert!(f.accepts(Lang::De as u8) && f.accepts(Lang::En as u8));
        assert!(!f.accepts(Lang::Other as u8));
        assert!(LangFilter::parse("all").accepts(Lang::Other as u8));
        assert!(!LangFilter::parse("de").accepts(Lang::En as u8));
    }
}
