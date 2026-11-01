//! Append-only Crawl-Speicher. Jede gecrawlte Seite wird als zstd-komprimierter
//! Record in Segmentdateien (`segment-NNNNNN.wcs`) abgelegt:
//! `[u32 LE Länge][zstd-Frame]`. Abgeschnittene Records am Dateiende (z.B. nach
//! einem Absturz) werden beim Lesen ignoriert.

use crate::codec::*;
use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

const RECORD_VERSION: u8 = 1;
const SEGMENT_MAX_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Link {
    pub url: String,
    pub anchor: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageRecord {
    /// Normalisierte URL nach Weiterleitungen.
    pub url: String,
    pub fetched_at: u64,
    pub status: u16,
    pub lang: u8,
    pub noindex: bool,
    pub canonical: Option<String>,
    pub title: String,
    pub description: String,
    pub headings: String,
    pub body: String,
    pub links: Vec<Link>,
}

impl PageRecord {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(self.body.len() + 256);
        b.push(RECORD_VERSION);
        write_str(&mut b, &self.url);
        write_varint(&mut b, self.fetched_at);
        write_varint(&mut b, self.status as u64);
        b.push(self.lang);
        b.push(self.noindex as u8);
        write_str(&mut b, self.canonical.as_deref().unwrap_or(""));
        write_str(&mut b, &self.title);
        write_str(&mut b, &self.description);
        write_str(&mut b, &self.headings);
        write_str(&mut b, &self.body);
        write_varint(&mut b, self.links.len() as u64);
        for l in &self.links {
            write_str(&mut b, &l.url);
            write_str(&mut b, &l.anchor);
        }
        b
    }

    pub fn decode(b: &[u8]) -> Result<PageRecord> {
        anyhow::ensure!(
            b.first() == Some(&RECORD_VERSION),
            "unbekannte Record-Version"
        );
        let mut p = 1;
        let url = read_str(b, &mut p)?.to_string();
        let fetched_at = read_varint(b, &mut p);
        let status = read_varint(b, &mut p) as u16;
        let lang = *b.get(p).context("kurz")?;
        let noindex = *b.get(p + 1).context("kurz")? != 0;
        p += 2;
        let canonical = read_str(b, &mut p)?;
        let canonical = (!canonical.is_empty()).then(|| canonical.to_string());
        let title = read_str(b, &mut p)?.to_string();
        let description = read_str(b, &mut p)?.to_string();
        let headings = read_str(b, &mut p)?.to_string();
        let body = read_str(b, &mut p)?.to_string();
        let n = read_varint(b, &mut p) as usize;
        let mut links = Vec::with_capacity(n.min(10_000));
        for _ in 0..n {
            let url = read_str(b, &mut p)?.to_string();
            let anchor = read_str(b, &mut p)?.to_string();
            links.push(Link { url, anchor });
        }
        Ok(PageRecord {
            url,
            fetched_at,
            status,
            lang,
            noindex,
            canonical,
            title,
            description,
            headings,
            body,
            links,
        })
    }
}

fn segment_path(dir: &Path, n: u32) -> PathBuf {
    dir.join(format!("segment-{n:06}.wcs"))
}

/// Alle Segmentdateien eines Verzeichnisses, sortiert.
pub fn list_segments(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut v = Vec::new();
    if !dir.exists() {
        return Ok(v);
    }
    for e in fs::read_dir(dir)? {
        let p = e?.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with("segment-") && name.ends_with(".wcs") {
            v.push(p);
        }
    }
    v.sort();
    Ok(v)
}

pub struct StoreWriter {
    dir: PathBuf,
    seg: u32,
    /// Wird erst beim ersten Record angelegt (keine leeren Segmente).
    out: Option<BufWriter<File>>,
    bytes: u64,
    compressor: zstd::bulk::Compressor<'static>,
}

impl StoreWriter {
    pub fn open(dir: &Path) -> Result<StoreWriter> {
        fs::create_dir_all(dir)?;
        let segs = list_segments(dir)?;
        // Für jeden Crawl-Lauf ein neues Segment: alte Segmente bleiben unverändert.
        let seg = segs
            .iter()
            .filter_map(|p| {
                p.file_stem()?
                    .to_str()?
                    .strip_prefix("segment-")?
                    .parse::<u32>()
                    .ok()
            })
            .max()
            .map_or(0, |m| m + 1);
        Ok(StoreWriter {
            dir: dir.to_path_buf(),
            seg,
            out: None,
            bytes: 0,
            compressor: zstd::bulk::Compressor::new(6)?,
        })
    }

    pub fn append(&mut self, rec: &PageRecord) -> Result<()> {
        let raw = rec.encode();
        let comp = self.compressor.compress(&raw)?;
        if self.bytes + comp.len() as u64 + 4 > SEGMENT_MAX_BYTES && self.bytes > 0 {
            if let Some(mut o) = self.out.take() {
                o.flush()?;
            }
            self.seg += 1;
            self.bytes = 0;
        }
        if self.out.is_none() {
            let f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(segment_path(&self.dir, self.seg))?;
            self.out = Some(BufWriter::with_capacity(1 << 20, f));
        }
        let out = self.out.as_mut().unwrap();
        out.write_all(&(comp.len() as u32).to_le_bytes())?;
        out.write_all(&comp)?;
        self.bytes += comp.len() as u64 + 4;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        if let Some(o) = self.out.as_mut() {
            o.flush()?;
        }
        Ok(())
    }
}

const MAX_RECORD_BYTES: usize = 64 << 20;

thread_local! {
    static DEC: std::cell::RefCell<zstd::bulk::Decompressor<'static>> =
        std::cell::RefCell::new(zstd::bulk::Decompressor::new().expect("zstd"));
}

/// Entpackt einen zstd-Frame mit exakt passendem Puffer (Größe steht im Frame-Header).
pub fn decompress_exact(
    dec: &mut zstd::bulk::Decompressor<'static>,
    comp: &[u8],
) -> Result<Vec<u8>> {
    let size = match zstd::zstd_safe::get_frame_content_size(comp) {
        Ok(Some(n)) if (n as usize) <= MAX_RECORD_BYTES => n as usize,
        Ok(Some(_)) => anyhow::bail!("Record zu groß"),
        _ => MAX_RECORD_BYTES,
    };
    let mut out = Vec::with_capacity(size);
    dec.decompress_to_buffer(comp, &mut out)?;
    Ok(out)
}

/// Position eines Records im Speicher (Segmentnummer, Byte-Offset des Längenfelds).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordLoc {
    pub seg: u32,
    pub off: u64,
}

/// Liest alle Records aller Segmente der Reihe nach. Callback erhält (Position, Record).
pub fn for_each_record<F: FnMut(RecordLoc, PageRecord) -> Result<()>>(
    dir: &Path,
    mut f: F,
) -> Result<()> {
    let mut dec = zstd::bulk::Decompressor::new()?;
    for (seg_no, seg) in list_segments(dir)?.into_iter().enumerate() {
        let mut off = 0u64;
        let mut r = BufReader::with_capacity(1 << 20, File::open(&seg)?);
        let mut len_buf = [0u8; 4];
        let mut comp = Vec::new();
        loop {
            if r.read_exact(&mut len_buf).is_err() {
                break;
            }
            let len = u32::from_le_bytes(len_buf) as usize;
            comp.resize(len, 0);
            if r.read_exact(&mut comp).is_err() {
                log::warn!(
                    "{}: abgeschnittener Record am Ende ignoriert",
                    seg.display()
                );
                break;
            }
            let loc = RecordLoc {
                seg: seg_no as u32,
                off,
            };
            off += 4 + len as u64;
            let raw = match decompress_exact(&mut dec, &comp) {
                Ok(r) => r,
                Err(e) => {
                    log::warn!("{}: defekter Record: {e}", seg.display());
                    continue;
                }
            };
            match PageRecord::decode(&raw) {
                Ok(rec) => f(loc, rec)?,
                Err(e) => log::warn!("{}: defekter Record: {e}", seg.display()),
            }
        }
    }
    Ok(())
}

/// Wahlfreier Zugriff auf Records über speichergemappte Segmente.
pub struct StoreReader {
    segs: Vec<memmap2::Mmap>,
}

impl StoreReader {
    pub fn open(dir: &Path) -> Result<StoreReader> {
        let mut segs = Vec::new();
        for p in list_segments(dir)? {
            let f = File::open(&p)?;
            // Sicherheit: Segmente werden nur angehängt, nie verändert.
            segs.push(unsafe { memmap2::Mmap::map(&f)? });
        }
        Ok(StoreReader { segs })
    }

    pub fn read(&self, loc: RecordLoc) -> Result<PageRecord> {
        let seg = self.segs.get(loc.seg as usize).context("Segment fehlt")?;
        let off = loc.off as usize;
        let len_bytes: [u8; 4] = seg
            .get(off..off + 4)
            .context("Offset ungültig")?
            .try_into()?;
        let len = u32::from_le_bytes(len_bytes) as usize;
        let comp = seg
            .get(off + 4..off + 4 + len)
            .context("Record abgeschnitten")?;
        let raw = DEC.with(|d| decompress_exact(&mut d.borrow_mut(), comp))?;
        PageRecord::decode(&raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let rec = PageRecord {
            url: "https://example.com/".into(),
            fetched_at: 123,
            status: 200,
            lang: 1,
            noindex: false,
            canonical: Some("https://example.com/".into()),
            title: "Titel".into(),
            description: "Beschreibung".into(),
            headings: "Ü1".into(),
            body: "Inhalt ".repeat(100),
            links: vec![Link {
                url: "https://example.com/a".into(),
                anchor: "A".into(),
            }],
        };
        {
            let mut w = StoreWriter::open(dir.path()).unwrap();
            w.append(&rec).unwrap();
            w.append(&rec).unwrap();
            w.flush().unwrap();
        }
        let mut locs = Vec::new();
        for_each_record(dir.path(), |loc, r| {
            assert_eq!(r, rec);
            locs.push(loc);
            Ok(())
        })
        .unwrap();
        assert_eq!(locs.len(), 2);
        let reader = StoreReader::open(dir.path()).unwrap();
        assert_eq!(reader.read(locs[1]).unwrap(), rec);
    }
}
