//! Kodierung und Dekodierung von Postinglisten.
//!
//! Layout einer Liste (ab dem im FST gespeicherten Offset):
//! ```text
//! varint df, varint nblocks
//! [nur wenn nblocks > 1] nblocks × { last_doc u32, block_end u32, pos_start u32 }
//! Blöcke: pack(doc-deltas) pack(tf_title) pack(tf_headings) pack(tf_body) pack(tf_url) pack(tf_anchor)
//! Positionen: pro Dokument pos_count(tf) varint-Deltas
//! ```
//! Doc-Deltas werden als `doc - prev - 1` gespeichert; Felder ohne Treffer kosten
//! pro Block nur 1 Byte.

use super::{pos_count, BLOCK, NUM_FIELDS};
use crate::codec::*;

pub const DOC_END: u32 = u32::MAX;

/// Ein Posting während des Aufbaus.
#[derive(Clone, Debug, PartialEq)]
pub struct Posting {
    pub doc: u32,
    pub tf: [u32; NUM_FIELDS],
    pub positions: Vec<u32>,
}

/// Streaming-Encoder: nimmt Postings einzeln (nach Doc-ID sortiert) entgegen und hält
/// nur die bereits komprimierten Bytes im Speicher.
pub struct ListEncoder {
    df: usize,
    prev: u32,
    buf: Vec<Posting>,
    body: Vec<u8>,
    pos_bytes: Vec<u8>,
    skips: Vec<(u32, u32, u32)>,
}

impl Default for ListEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl ListEncoder {
    pub fn new() -> ListEncoder {
        ListEncoder {
            df: 0,
            prev: DOC_END,
            buf: Vec::with_capacity(BLOCK),
            body: Vec::new(),
            pos_bytes: Vec::new(),
            skips: Vec::new(),
        }
    }

    pub fn push(&mut self, p: Posting) {
        self.buf.push(p);
        self.df += 1;
        if self.buf.len() == BLOCK {
            self.flush_block();
        }
    }

    fn flush_block(&mut self) {
        let n = self.buf.len();
        if n == 0 {
            return;
        }
        let mut vals = [0u32; BLOCK];
        let pos_start = self.pos_bytes.len() as u32;
        let mut prev = self.prev;
        for (i, p) in self.buf.iter().enumerate() {
            vals[i] = p.doc.wrapping_sub(prev).wrapping_sub(1);
            prev = p.doc;
        }
        self.prev = prev;
        pack(&mut self.body, &vals[..n]);
        for f in 0..NUM_FIELDS {
            for (i, p) in self.buf.iter().enumerate() {
                vals[i] = p.tf[f];
            }
            pack(&mut self.body, &vals[..n]);
        }
        for p in &self.buf {
            let cnt = pos_count(&p.tf) as usize;
            let mut last = 0u32;
            for (j, &pos) in p.positions.iter().take(cnt).enumerate() {
                write_varint(
                    &mut self.pos_bytes,
                    (if j == 0 { pos } else { pos - last }) as u64,
                );
                last = pos;
            }
            // Falls weniger Positionen vorliegen als erwartet, mit 0-Deltas auffüllen.
            for _ in p.positions.len().min(cnt)..cnt {
                write_varint(&mut self.pos_bytes, 0);
            }
        }
        self.skips.push((prev, self.body.len() as u32, pos_start));
        self.buf.clear();
    }

    pub fn df(&self) -> usize {
        self.df
    }

    /// Schreibt die fertige Liste nach `out`.
    pub fn finish(mut self, out: &mut Vec<u8>) {
        self.flush_block();
        let nblocks = self.skips.len();
        write_varint(out, self.df as u64);
        write_varint(out, nblocks as u64);
        if nblocks > 1 {
            for (last, end, ps) in &self.skips {
                out.extend_from_slice(&last.to_le_bytes());
                out.extend_from_slice(&end.to_le_bytes());
                out.extend_from_slice(&ps.to_le_bytes());
            }
        }
        out.extend_from_slice(&self.body);
        out.extend_from_slice(&self.pos_bytes);
    }
}

/// Kodiert eine nach Doc-ID sortierte Postingliste.
pub fn encode_list(postings: &[Posting], out: &mut Vec<u8>) {
    let mut e = ListEncoder::new();
    for p in postings {
        e.push(p.clone());
    }
    e.finish(out);
}

/// Liest nur die Dokumentfrequenz einer Liste.
pub fn read_df(data: &[u8]) -> u32 {
    let mut p = 0;
    read_varint(data, &mut p) as u32
}

/// Iterator über eine Postingliste mit Skip-Unterstützung und lazy Positionen.
pub struct Cursor<'a> {
    data: &'a [u8],
    pub df: u32,
    nblocks: usize,
    skip_start: usize,
    blocks_start: usize,
    positions_start: usize,
    block: usize,
    idx: usize,
    n: usize,
    docs: [u32; BLOCK],
    tfs: [[u32; BLOCK]; NUM_FIELDS],
    doc: u32,
    // Lazy-Positionen: (Dokumentindex im Block, Byte-Offset)
    pos_idx: usize,
    pos_off: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(data: &'a [u8]) -> Cursor<'a> {
        let mut p = 0;
        let df = read_varint(data, &mut p) as u32;
        let nblocks = read_varint(data, &mut p) as usize;
        let skip_start = p;
        let blocks_start = if nblocks > 1 { p + nblocks * 12 } else { p };
        let mut c = Cursor {
            data,
            df,
            nblocks,
            skip_start,
            blocks_start,
            positions_start: 0,
            block: 0,
            idx: 0,
            n: 0,
            docs: [0; BLOCK],
            tfs: [[0; BLOCK]; NUM_FIELDS],
            doc: DOC_END,
            pos_idx: 0,
            pos_off: 0,
        };
        if nblocks > 1 {
            c.positions_start = blocks_start + c.skip(nblocks - 1).1 as usize;
        }
        if df > 0 && nblocks > 0 {
            c.load_block(0);
        }
        c
    }

    #[inline]
    fn skip(&self, b: usize) -> (u32, u32, u32) {
        let at = self.skip_start + b * 12;
        let r = |o: usize| u32::from_le_bytes(self.data[at + o..at + o + 4].try_into().unwrap());
        (r(0), r(4), r(8))
    }

    fn load_block(&mut self, b: usize) {
        self.block = b;
        let (mut p, prev) = if b == 0 {
            (self.blocks_start, DOC_END)
        } else {
            let (last, end, _) = self.skip(b - 1);
            (self.blocks_start + end as usize, last)
        };
        let n = if b + 1 == self.nblocks {
            self.df as usize - b * BLOCK
        } else {
            BLOCK
        };
        self.n = n;
        unpack(self.data, &mut p, &mut self.docs[..n]);
        let mut d = prev;
        for i in 0..n {
            d = d.wrapping_add(self.docs[i]).wrapping_add(1);
            self.docs[i] = d;
        }
        for f in 0..NUM_FIELDS {
            unpack(self.data, &mut p, &mut self.tfs[f][..n]);
        }
        if self.nblocks == 1 {
            self.positions_start = p;
        }
        self.idx = 0;
        self.doc = self.docs[0];
        self.pos_idx = 0;
        self.pos_off = self.positions_start
            + if self.nblocks > 1 {
                self.skip(b).2 as usize
            } else {
                0
            };
    }

    #[inline]
    pub fn doc(&self) -> u32 {
        self.doc
    }

    #[inline]
    pub fn tf(&self, field: usize) -> u32 {
        self.tfs[field][self.idx]
    }

    pub fn tfs(&self) -> [u32; NUM_FIELDS] {
        let mut t = [0; NUM_FIELDS];
        for (f, v) in t.iter_mut().enumerate() {
            *v = self.tfs[f][self.idx];
        }
        t
    }

    pub fn next_doc(&mut self) -> u32 {
        if self.doc == DOC_END {
            return DOC_END;
        }
        self.idx += 1;
        if self.idx < self.n {
            self.doc = self.docs[self.idx];
        } else if self.block + 1 < self.nblocks {
            self.load_block(self.block + 1);
        } else {
            self.doc = DOC_END;
        }
        self.doc
    }

    /// Springt zum ersten Dokument ≥ `target`.
    pub fn advance(&mut self, target: u32) -> u32 {
        if self.doc >= target {
            return self.doc;
        }
        if self.nblocks > 1 {
            let (last, _, _) = self.skip(self.block);
            if last < target {
                // Binärsuche über die Skip-Tabelle
                let (mut lo, mut hi) = (self.block + 1, self.nblocks);
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    if self.skip(mid).0 < target {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                if lo >= self.nblocks {
                    self.doc = DOC_END;
                    return DOC_END;
                }
                self.load_block(lo);
            }
        }
        while self.doc < target {
            if self.next_doc() == DOC_END {
                break;
            }
        }
        self.doc
    }

    /// Positionen des aktuellen Dokuments (Titel: 0.., Fließtext: ab Titel + Abstand).
    pub fn positions(&mut self, out: &mut Vec<u32>) {
        out.clear();
        if self.doc == DOC_END {
            return;
        }
        if self.pos_idx > self.idx {
            // zurücksetzen (sollte bei Vorwärts-Iteration nicht vorkommen)
            self.pos_idx = 0;
            self.pos_off = self.positions_start
                + if self.nblocks > 1 {
                    self.skip(self.block).2 as usize
                } else {
                    0
                };
        }
        while self.pos_idx < self.idx {
            let i = self.pos_idx;
            let cnt = (self.tfs[super::F_TITLE][i] + self.tfs[super::F_BODY][i])
                .min(super::MAX_POSITIONS);
            skip_varints(self.data, &mut self.pos_off, cnt as usize);
            self.pos_idx += 1;
        }
        let cnt = pos_count(&self.tfs()) as usize;
        let mut p = self.pos_off;
        let mut last = 0u32;
        for j in 0..cnt {
            let v = read_varint(self.data, &mut p) as u32;
            last = if j == 0 { v } else { last + v };
            out.push(last);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(n: usize, step: u32) -> Vec<Posting> {
        (0..n)
            .map(|i| {
                let doc = i as u32 * step + (i as u32 % 3);
                let tf = [
                    (i % 2) as u32,
                    0,
                    (i % 5) as u32 + 1,
                    0,
                    (i % 7 == 0) as u32,
                ];
                let cnt = pos_count(&tf);
                let positions = (0..cnt).map(|j| j * 3 + i as u32 % 4).collect();
                Posting { doc, tf, positions }
            })
            .collect()
    }

    #[test]
    fn roundtrip_iterate() {
        for n in [1usize, 2, 127, 128, 129, 1000] {
            let ps = mk(n, 7);
            let mut buf = vec![9, 9, 9];
            encode_list(&ps, &mut buf);
            buf.extend_from_slice(&[1, 2, 3]);
            let mut c = Cursor::new(&buf[3..]);
            assert_eq!(c.df as usize, n);
            let mut pos = Vec::new();
            for p in &ps {
                assert_eq!(c.doc(), p.doc);
                assert_eq!(c.tfs(), p.tf);
                c.positions(&mut pos);
                assert_eq!(pos, p.positions, "doc {}", p.doc);
                c.next_doc();
            }
            assert_eq!(c.doc(), DOC_END);
        }
    }

    #[test]
    fn advance_and_positions() {
        let ps = mk(1000, 10);
        let mut buf = Vec::new();
        encode_list(&ps, &mut buf);
        let mut c = Cursor::new(&buf);
        let mut pos = Vec::new();
        for target in [0u32, 5, 500, 1281, 1290, 5000, 9990] {
            let expect = ps.iter().find(|p| p.doc >= target);
            let got = c.advance(target);
            match expect {
                Some(p) => {
                    assert_eq!(got, p.doc);
                    c.positions(&mut pos);
                    assert_eq!(pos, p.positions);
                }
                None => assert_eq!(got, DOC_END),
            }
        }
        assert_eq!(c.advance(1_000_000), DOC_END);
    }

    #[test]
    fn compact() {
        // 10 000 aufeinanderfolgende Dokumente mit tf=1 im Fließtext
        let ps: Vec<Posting> = (0..10_000)
            .map(|d| Posting {
                doc: d,
                tf: [0, 0, 1, 0, 0],
                positions: vec![17],
            })
            .collect();
        let mut buf = Vec::new();
        encode_list(&ps, &mut buf);
        // Deltas: 0 Bit, tf_body: 1 Bit, Positionen: 1 Byte → ~1,2 Byte pro Posting
        assert!(buf.len() < 14_000, "{} Bytes", buf.len());
    }
}
