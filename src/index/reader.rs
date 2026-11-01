//! Lesezugriff auf einen fertigen Index. Alle großen Dateien werden per `mmap`
//! eingebunden: das Betriebssystem lädt nur die tatsächlich benötigten Seiten,
//! der Prozess selbst braucht kaum eigenen Speicher.

use super::docstore::DocStore;
use super::postings::{read_df, Cursor};
use super::*;
use crate::codec::*;
use anyhow::{Context, Result};
use fst::{IntoStreamer, Streamer};
use memmap2::Mmap;
use std::fs::File;
use std::path::{Path, PathBuf};

pub struct Index {
    pub dir: PathBuf,
    pub meta: IndexMeta,
    terms: fst::Map<Mmap>,
    words: fst::Map<Mmap>,
    postings: Mmap,
    docmeta: Mmap,
    pub docs: DocStore,
    /// (umgekehrter Host, erste Doc-ID, Anzahl)
    hosts: Vec<(String, u32, u32)>,
    len_table: [f32; 256],
}

fn map_file(p: &Path) -> Result<Mmap> {
    let f = File::open(p).with_context(|| format!("{} fehlt", p.display()))?;
    // Sicherheit: Indexdateien werden nach dem Aufbau nie verändert (nur ersetzt).
    Ok(unsafe { Mmap::map(&f)? })
}

impl Index {
    pub fn open(dir: &Path) -> Result<Index> {
        let meta: IndexMeta = serde_json::from_slice(
            &std::fs::read(dir.join("meta.json"))
                .with_context(|| format!("Kein Index in {}", dir.display()))?,
        )?;
        anyhow::ensure!(
            meta.version == FORMAT_VERSION,
            "Indexformat {} wird nicht unterstützt",
            meta.version
        );
        let terms = fst::Map::new(map_file(&dir.join("terms.fst"))?)?;
        let words = fst::Map::new(map_file(&dir.join("words.fst"))?)?;
        let postings = map_file(&dir.join("postings.bin"))?;
        let docmeta = map_file(&dir.join("docmeta.bin"))?;
        anyhow::ensure!(
            docmeta.len() == meta.num_docs as usize * DOCMETA_SIZE,
            "docmeta.bin hat falsche Größe"
        );
        let docs = DocStore::open(dir)?;
        let hb = std::fs::read(dir.join("hosts.bin"))?;
        let mut p = 0;
        let n = read_varint(&hb, &mut p) as usize;
        let mut hosts = Vec::with_capacity(n);
        for _ in 0..n {
            let h = read_str(&hb, &mut p)?.to_string();
            let start = read_varint(&hb, &mut p) as u32;
            let cnt = read_varint(&hb, &mut p) as u32;
            hosts.push((h, start, cnt));
        }
        let mut len_table = [0f32; 256];
        for (i, v) in len_table.iter_mut().enumerate() {
            *v = dequantize_len(i as u8);
        }
        Ok(Index {
            dir: dir.to_path_buf(),
            meta,
            terms,
            words,
            postings,
            docmeta,
            docs,
            hosts,
            len_table,
        })
    }

    pub fn num_docs(&self) -> u32 {
        self.meta.num_docs
    }

    #[inline]
    pub fn field_len(&self, q: u8) -> f32 {
        self.len_table[q as usize]
    }

    fn list(&self, term: &str) -> Option<&[u8]> {
        let off = self.terms.get(term.as_bytes())? as usize;
        self.postings.get(off..)
    }

    pub fn cursor(&self, term: &str) -> Option<Cursor<'_>> {
        self.list(term).map(Cursor::new)
    }

    pub fn df(&self, term: &str) -> u32 {
        self.list(term).map(read_df).unwrap_or(0)
    }

    pub fn doc_meta(&self, doc: u32) -> DocMeta {
        let o = doc as usize * DOCMETA_SIZE;
        DocMeta::from_bytes(&self.docmeta[o..o + DOCMETA_SIZE])
    }

    /// Umgekehrter Host-Schlüssel eines Dokuments (z.B. `org.wikipedia.de`).
    pub fn host_of(&self, doc: u32) -> &str {
        let i = self.hosts.partition_point(|h| h.1 <= doc);
        if i == 0 {
            return "";
        }
        &self.hosts[i - 1].0
    }

    /// Doc-ID-Bereiche für `site:example.com` (inkl. Subdomains), sortiert.
    pub fn site_ranges(&self, site: &str) -> Vec<(u32, u32)> {
        let site = site
            .trim()
            .trim_start_matches("www.")
            .trim_end_matches('/')
            .to_ascii_lowercase();
        let rev = crate::urlnorm::reverse_host(&site);
        let sub = format!("{rev}.");
        let mut out: Vec<(u32, u32)> = Vec::new();
        let start = self.hosts.partition_point(|h| h.0.as_str() < rev.as_str());
        for h in &self.hosts[start..] {
            if !h.0.starts_with(rev.as_str()) {
                break;
            }
            if h.0 == rev || h.0.starts_with(&sub) {
                let (a, b) = (h.1, h.1 + h.2);
                match out.last_mut() {
                    Some(last) if last.1 == a => last.1 = b,
                    _ => out.push((a, b)),
                }
            }
        }
        out
    }

    /// Dokumentfrequenz eines Oberflächenworts.
    pub fn word_df(&self, word: &str) -> u32 {
        self.words.get(word.as_bytes()).unwrap_or(0) as u32
    }

    /// Wörter mit Editierdistanz ≤ `dist` (für „Meinten Sie“).
    pub fn fuzzy_words(&self, word: &str, dist: u32) -> Vec<(String, u32)> {
        let Ok(aut) = fst::automaton::Levenshtein::new(word, dist) else {
            return Vec::new();
        };
        let mut s = self.words.search(aut).into_stream();
        let mut out = Vec::new();
        while let Some((k, v)) = s.next() {
            if let Ok(k) = std::str::from_utf8(k) {
                out.push((k.to_string(), v as u32));
            }
            if out.len() > 2000 {
                break;
            }
        }
        out
    }

    /// Häufigste Wörter mit gegebenem Präfix (Autovervollständigung).
    pub fn complete_word(&self, prefix: &str, limit: usize) -> Vec<(String, u32)> {
        if prefix.is_empty() {
            return Vec::new();
        }
        let mut s = self.words.range().ge(prefix.as_bytes()).into_stream();
        let mut all = Vec::new();
        let mut scanned = 0;
        while let Some((k, v)) = s.next() {
            if !k.starts_with(prefix.as_bytes()) || scanned > 20_000 {
                break;
            }
            scanned += 1;
            if let Ok(k) = std::str::from_utf8(k) {
                all.push((k.to_string(), v as u32));
            }
        }
        all.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.len().cmp(&b.0.len())));
        all.truncate(limit);
        all
    }

    /// Größen der Indexdateien in Bytes.
    pub fn file_sizes(&self) -> Vec<(&'static str, u64)> {
        let f = |n: &str| {
            std::fs::metadata(self.dir.join(n))
                .map(|m| m.len())
                .unwrap_or(0)
        };
        vec![
            ("terms.fst", f("terms.fst")),
            ("postings.bin", f("postings.bin")),
            ("docmeta.bin", f("docmeta.bin")),
            ("docstore.bin", f("docstore.bin")),
            ("docstore.idx", f("docstore.idx")),
            ("hosts.bin", f("hosts.bin")),
            ("words.fst", f("words.fst")),
        ]
    }

    pub fn num_hosts(&self) -> usize {
        self.hosts.len()
    }
}
