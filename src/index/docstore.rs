//! Komprimierter Dokumentspeicher: je 16 Dokumente bilden einen zstd-Block.
//! Blöcke komprimieren besser als einzelne Dokumente (gemeinsame Textmuster,
//! gleiche Domain wegen der Doc-ID-Sortierung) und bleiben trotzdem schnell lesbar.

use crate::codec::*;
use anyhow::{Context, Result};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

pub const DOCS_PER_BLOCK: usize = 16;
/// So viel Fließtext wird für Snippets aufbewahrt.
pub const STORED_BODY_BYTES: usize = 12_000;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct StoredDoc {
    pub url: String,
    pub title: String,
    pub description: String,
    pub body: String,
}

impl StoredDoc {
    fn encode(&self, out: &mut Vec<u8>) {
        write_str(out, &self.url);
        write_str(out, &self.title);
        write_str(out, &self.description);
        write_str(out, &self.body);
    }
    fn decode(b: &[u8], p: &mut usize) -> Result<StoredDoc> {
        Ok(StoredDoc {
            url: read_str(b, p)?.to_string(),
            title: read_str(b, p)?.to_string(),
            description: read_str(b, p)?.to_string(),
            body: read_str(b, p)?.to_string(),
        })
    }
}

const ZSTD_LEVEL: i32 = 12;

pub struct DocStoreWriter {
    data: BufWriter<File>,
    idx: BufWriter<File>,
    offset: u64,
    block: Vec<u8>,
    in_block: usize,
}

impl DocStoreWriter {
    pub fn create(dir: &Path) -> Result<DocStoreWriter> {
        Ok(DocStoreWriter {
            data: BufWriter::new(File::create(dir.join("docstore.bin"))?),
            idx: BufWriter::new(File::create(dir.join("docstore.idx"))?),
            offset: 0,
            block: Vec::new(),
            in_block: 0,
        })
    }

    /// Fügt viele Dokumente hinzu und komprimiert volle Blöcke parallel.
    pub fn add_many(&mut self, docs: &[StoredDoc]) -> Result<()> {
        use rayon::prelude::*;
        let mut i = 0;
        // Angefangenen Block zuerst auffüllen
        while self.in_block != 0 && i < docs.len() {
            self.add(&docs[i])?;
            i += 1;
        }
        let full = (docs.len() - i) / DOCS_PER_BLOCK * DOCS_PER_BLOCK;
        let blocks: Vec<Result<Vec<u8>>> = docs[i..i + full]
            .par_chunks(DOCS_PER_BLOCK)
            .map(|chunk| {
                let mut raw = Vec::new();
                for d in chunk {
                    d.encode(&mut raw);
                }
                Ok(zstd::bulk::compress(&raw, ZSTD_LEVEL)?)
            })
            .collect();
        for b in blocks {
            self.write_block(&b?)?;
        }
        for d in &docs[i + full..] {
            self.add(d)?;
        }
        Ok(())
    }

    fn write_block(&mut self, c: &[u8]) -> Result<()> {
        self.idx.write_all(&self.offset.to_le_bytes())?;
        self.data.write_all(c)?;
        self.offset += c.len() as u64;
        Ok(())
    }

    pub fn add(&mut self, doc: &StoredDoc) -> Result<()> {
        doc.encode(&mut self.block);
        self.in_block += 1;
        if self.in_block == DOCS_PER_BLOCK {
            self.flush_block()?;
        }
        Ok(())
    }

    fn flush_block(&mut self) -> Result<()> {
        if self.in_block == 0 {
            return Ok(());
        }
        let c = zstd::bulk::compress(&self.block, ZSTD_LEVEL)?;
        self.write_block(&c)?;
        self.block.clear();
        self.in_block = 0;
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        self.flush_block()?;
        self.idx.write_all(&self.offset.to_le_bytes())?;
        self.data.flush()?;
        self.idx.flush()?;
        Ok(())
    }
}

pub struct DocStore {
    data: memmap2::Mmap,
    idx: memmap2::Mmap,
}

impl DocStore {
    pub fn open(dir: &Path) -> Result<DocStore> {
        let data = unsafe { memmap2::Mmap::map(&File::open(dir.join("docstore.bin"))?)? };
        let idx = unsafe { memmap2::Mmap::map(&File::open(dir.join("docstore.idx"))?)? };
        Ok(DocStore { data, idx })
    }

    fn off(&self, i: usize) -> Option<usize> {
        let b = self.idx.get(i * 8..i * 8 + 8)?;
        Some(u64::from_le_bytes(b.try_into().ok()?) as usize)
    }

    pub fn get(&self, doc: u32) -> Result<StoredDoc> {
        let block = doc as usize / DOCS_PER_BLOCK;
        let (a, b) = (
            self.off(block).context("Doc-ID ungültig")?,
            self.off(block + 1).context("Doc-ID ungültig")?,
        );
        let raw = {
            let mut d = zstd::bulk::Decompressor::new()?;
            crate::store::decompress_exact(
                &mut d,
                self.data.get(a..b).context("Docstore beschädigt")?,
            )?
        };
        let mut p = 0;
        for _ in 0..doc as usize % DOCS_PER_BLOCK {
            StoredDoc::decode(&raw, &mut p)?;
        }
        anyhow::ensure!(p < raw.len(), "Doc-ID ungültig");
        StoredDoc::decode(&raw, &mut p)
    }

    pub fn size_bytes(&self) -> u64 {
        (self.data.len() + self.idx.len()) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let docs: Vec<StoredDoc> = (0..40)
            .map(|i| StoredDoc {
                url: format!("https://example.com/{i}"),
                title: format!("Titel {i}"),
                description: String::new(),
                body: format!("Text {i} ").repeat(i + 1),
            })
            .collect();
        let mut w = DocStoreWriter::create(dir.path()).unwrap();
        w.add(&docs[0]).unwrap();
        w.add_many(&docs[1..37]).unwrap();
        for d in &docs[37..] {
            w.add(d).unwrap();
        }
        w.finish().unwrap();
        let r = DocStore::open(dir.path()).unwrap();
        for (i, d) in docs.iter().enumerate() {
            assert_eq!(&r.get(i as u32).unwrap(), d);
        }
        assert!(r.get(40).is_err());
    }
}
