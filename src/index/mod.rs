//! Eigener, kompakter invertierter Index.
//!
//! Dateien im Indexverzeichnis:
//! * `terms.fst`     – Wörterbuch als Finite State Transducer (Term → Offset in Postings).
//!   Gemeinsame Präfixe *und* Suffixe werden geteilt → sehr klein.
//! * `postings.bin`  – pro Term: Blöcke à 128 Dokumente, Doc-ID-Deltas und
//!   Termfrequenzen je Feld bitgepackt, plus Skip-Tabelle und
//!   delta-kodierte Positionen (für Phrasen und Nähe).
//! * `docmeta.bin`   – 8 Byte pro Dokument: quantisierte Feldlängen, PageRank, Sprache.
//! * `docstore.bin`/`.idx` – zstd-komprimierte Blöcke mit URL, Titel, Text (für Snippets).
//! * `hosts.bin`     – Host → Doc-ID-Bereich (Doc-IDs sind nach umgekehrtem Host sortiert).
//! * `meta.json`     – Statistiken (Anzahl Dokumente, mittlere Feldlängen).

pub mod builder;
pub mod docstore;
pub mod pagerank;
pub mod postings;
pub mod reader;

pub use reader::Index;

/// Indexfelder.
pub const F_TITLE: usize = 0;
pub const F_HEADINGS: usize = 1;
pub const F_BODY: usize = 2;
pub const F_URL: usize = 3;
pub const F_ANCHOR: usize = 4;
pub const NUM_FIELDS: usize = 5;
pub const FIELD_NAMES: [&str; NUM_FIELDS] = ["title", "headings", "body", "url", "anchor"];

/// Höchstens so viele Positionen werden pro Term und Dokument gespeichert.
pub const MAX_POSITIONS: u32 = 64;
/// Positionen des Fließtexts beginnen hinter dem Titel plus diesem Abstand,
/// damit Phrasen nicht über die Feldgrenze hinweg matchen.
pub const BODY_POS_GAP: u32 = 16;
pub const BLOCK: usize = 128;
pub const FORMAT_VERSION: u32 = 1;

/// Anzahl gespeicherter Positionen für gegebene Termfrequenzen.
#[inline]
pub fn pos_count(tf: &[u32; NUM_FIELDS]) -> u32 {
    (tf[F_TITLE] + tf[F_BODY]).min(MAX_POSITIONS)
}

/// Längen logarithmisch auf 1 Byte quantisieren (≈4 % Auflösung, bis ~65 000).
#[inline]
pub fn quantize_len(len: u32) -> u8 {
    ((len as f32 + 1.0).ln() * 23.0).round().clamp(0.0, 255.0) as u8
}

#[inline]
pub fn dequantize_len(q: u8) -> f32 {
    ((q as f32) / 23.0).exp() - 1.0
}

/// 8-Byte-Metadaten pro Dokument.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DocMeta {
    pub len_q: [u8; NUM_FIELDS],
    /// PageRank, logarithmisch auf 0..=255 abgebildet.
    pub quality: u8,
    pub lang: u8,
    /// Bits 0..4: URL-Tiefe (max. 15), Bit 4: Startseite.
    pub flags: u8,
}

pub const DOCMETA_SIZE: usize = 8;

impl DocMeta {
    pub fn to_bytes(&self) -> [u8; DOCMETA_SIZE] {
        let mut b = [0u8; DOCMETA_SIZE];
        b[..NUM_FIELDS].copy_from_slice(&self.len_q);
        b[5] = self.quality;
        b[6] = self.lang;
        b[7] = self.flags;
        b
    }
    pub fn from_bytes(b: &[u8]) -> DocMeta {
        let mut len_q = [0u8; NUM_FIELDS];
        len_q.copy_from_slice(&b[..NUM_FIELDS]);
        DocMeta {
            len_q,
            quality: b[5],
            lang: b[6],
            flags: b[7],
        }
    }
    pub fn depth(&self) -> u8 {
        self.flags & 0x0f
    }
    pub fn is_home(&self) -> bool {
        self.flags & 0x10 != 0
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct IndexMeta {
    pub version: u32,
    pub num_docs: u32,
    pub num_terms: u64,
    pub avg_len: [f32; NUM_FIELDS],
    pub built_at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantization() {
        for len in [0u32, 1, 5, 10, 100, 1000, 10_000, 60_000] {
            let d = dequantize_len(quantize_len(len));
            let err = (d - len as f32).abs() / (len as f32 + 1.0);
            assert!(err < 0.05, "{len} -> {d}");
        }
        let m = DocMeta {
            len_q: [1, 2, 3, 4, 5],
            quality: 6,
            lang: 1,
            flags: 0x13,
        };
        assert_eq!(DocMeta::from_bytes(&m.to_bytes()), m);
        assert_eq!(m.depth(), 3);
        assert!(m.is_home());
    }
}
