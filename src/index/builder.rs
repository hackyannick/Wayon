//! Indexaufbau aus dem Crawl-Speicher.
//!
//! 1. **Auswahl:** neueste Version jeder URL, `noindex`/Fehlerseiten raus,
//!    `rel=canonical` und inhaltsgleiche Duplikate zusammenfassen.
//! 2. **Doc-IDs** nach umgekehrtem Host + Pfad vergeben (bessere Kompression, `site:`).
//! 3. **Linkgraph:** Ankertexte sammeln, PageRank berechnen.
//! 4. **Invertieren (SPIMI):** Dokumente parallel analysieren, Postings im Speicher
//!    sammeln, bei Erreichen des Speicherbudgets als sortierten Lauf auf Platte
//!    schreiben, am Ende per k-Wege-Merge in das finale, bitgepackte Format überführen.

use super::docstore::{DocStoreWriter, StoredDoc, STORED_BODY_BYTES};
use super::pagerank::{self, LinkGraph, INTERNAL_BIT};
use super::postings::{ListEncoder, Posting, DOC_END};
use super::*;
use crate::codec::*;
use crate::crawler::prefix;
use crate::store::{self, RecordLoc, StoreReader};
use crate::text::{self, Lang};
use crate::urlnorm;
use anyhow::{Context, Result};
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;
use url::Url;
use xxhash_rust::xxh3::xxh3_64;

#[derive(Clone, Debug)]
pub struct BuildConfig {
    pub store_dir: PathBuf,
    pub index_dir: PathBuf,
    /// Speicherbudget für Postings im RAM, bevor auf Platte ausgelagert wird.
    pub mem_budget_bytes: usize,
}

const CHUNK: usize = 512;
const MAX_ANCHORS_PER_DOC: usize = 24;
const MAX_ANCHOR_BYTES_PER_DOC: usize = 3000;

const GENERIC_ANCHORS: &[&str] = &[
    "hier",
    "here",
    "mehr",
    "more",
    "weiter",
    "weiterlesen",
    "read more",
    "mehr erfahren",
    "click here",
    "hier klicken",
    "link",
    "zurück",
    "back",
    "next",
    "weiter »",
    "»",
    "«",
    "...",
    "mehr lesen",
    "learn more",
    "details",
    "home",
    "startseite",
];

struct Cand {
    loc: RecordLoc,
    fetched_at: u64,
    /// Hash der kanonischen URL (0 = keine)
    canonical: u64,
    content_hash: u64,
    url: String,
}

#[inline]
fn url_hash(u: &str) -> u64 {
    xxh3_64(u.as_bytes()).max(1)
}

/// Ankertexte eines Zieldokuments, kompakt kodiert als Folge von
/// `[Anzahl u8][varint Länge][UTF-8]` – ein einziger Heap-Block pro Dokument.
type AnchorBuf = Vec<u8>;

fn anchor_iter(buf: &[u8]) -> impl Iterator<Item = (u32, &str)> {
    let mut p = 0;
    std::iter::from_fn(move || {
        if p >= buf.len() {
            return None;
        }
        let cnt = buf[p] as u32;
        p += 1;
        let s = read_str(buf, &mut p).ok()?;
        Some((cnt, s))
    })
}

fn anchor_add(buf: &mut AnchorBuf, text: &str) {
    let mut p = 0;
    let mut n = 0;
    while p < buf.len() {
        let cnt_at = p;
        p += 1;
        let Ok(s) = read_str(buf, &mut p) else { return };
        if s.eq_ignore_ascii_case(text) {
            buf[cnt_at] = buf[cnt_at].saturating_add(1);
            return;
        }
        n += 1;
    }
    if n < MAX_ANCHORS_PER_DOC && buf.len() + text.len() <= MAX_ANCHOR_BYTES_PER_DOC {
        buf.push(1);
        write_str(buf, text);
    }
}

type TermEntry = (String, [u32; NUM_FIELDS], Vec<u32>);

struct DocOut {
    /// Terme und Oberflächenwörter, bereits nach Shard (Term-Hash) aufgeteilt.
    shards: Vec<(Vec<TermEntry>, Vec<String>)>,
    lens: [u32; NUM_FIELDS],
    meta: DocMeta,
    stored: StoredDoc,
}

pub fn build(cfg: &BuildConfig) -> Result<IndexMeta> {
    let t0 = Instant::now();
    // ---------- 1. Auswahl ----------
    // URLs werden als 64-Bit-Hash geführt (spart gegenüber Strings ~80 % Speicher).
    let mut latest: FxHashMap<u64, Cand> = FxHashMap::default();
    let mut total = 0u64;
    store::for_each_record(&cfg.store_dir, |loc, r| {
        total += 1;
        let key = url_hash(&r.url);
        if r.status != 200 || r.noindex {
            if latest
                .get(&key)
                .is_some_and(|c| c.fetched_at <= r.fetched_at)
            {
                latest.remove(&key);
            }
            return Ok(());
        }
        let content_hash = if r.body.len() >= 64 {
            xxh3_64(r.body.as_bytes())
        } else {
            0
        };
        if latest
            .get(&key)
            .is_none_or(|c| c.fetched_at <= r.fetched_at)
        {
            let canonical = r
                .canonical
                .as_deref()
                .map(url_hash)
                .filter(|&c| c != key)
                .unwrap_or(0);
            latest.insert(
                key,
                Cand {
                    loc,
                    fetched_at: r.fetched_at,
                    canonical,
                    content_hash,
                    url: r.url,
                },
            );
        }
        Ok(())
    })?;
    anyhow::ensure!(
        !latest.is_empty(),
        "Keine indexierbaren Seiten im Crawl-Speicher {}",
        cfg.store_dir.display()
    );

    // Kanonische URLs und Duplikate → Aliase (Alias-Hash → Ziel-Hash).
    let mut alias: FxHashMap<u64, u64> = FxHashMap::default();
    let canon: Vec<(u64, u64)> = latest
        .iter()
        .filter(|(_, c)| c.canonical != 0 && latest.contains_key(&c.canonical))
        .map(|(&u, c)| (u, c.canonical))
        .collect();
    for (u, cu) in canon {
        // Nur einstufig: das Ziel darf selbst nicht auf etwas anderes zeigen.
        if alias.contains_key(&cu) {
            continue;
        }
        latest.remove(&u);
        alias.insert(u, cu);
    }
    let mut by_hash: FxHashMap<u64, u64> = FxHashMap::default();
    let mut dups = Vec::new();
    for (&u, c) in &latest {
        if c.content_hash == 0 {
            continue;
        }
        match by_hash.get_mut(&c.content_hash) {
            None => {
                by_hash.insert(c.content_hash, u);
            }
            Some(rep) => {
                // Kürzeste URL wird Repräsentant
                let (cu, cr) = (&c.url, &latest[rep].url);
                if (cu.len(), cu.as_str()) < (cr.len(), cr.as_str()) {
                    dups.push((std::mem::replace(rep, u), u));
                } else {
                    dups.push((u, *rep));
                }
            }
        }
    }
    drop(by_hash);
    for (d, _) in &dups {
        latest.remove(d);
    }
    alias.extend(dups);

    // ---------- 2. Doc-IDs ----------
    let mut docs: Vec<(String, u64, RecordLoc)> = latest
        .into_iter()
        .filter_map(|(u, c)| {
            Url::parse(&c.url)
                .ok()
                .map(|pu| (urlnorm::sort_key(&pu), u, c.loc))
        })
        .collect();
    docs.par_sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let n = docs.len();
    anyhow::ensure!(n < (INTERNAL_BIT as usize), "zu viele Dokumente");
    let mut url_to_doc: FxHashMap<u64, u32> = FxHashMap::default();
    url_to_doc.reserve(n + alias.len());
    for (i, d) in docs.iter().enumerate() {
        url_to_doc.insert(d.1, i as u32);
    }
    for (&a, &r) in &alias {
        let mut target = r;
        // Alias-Ketten (max. 4 Stufen) verfolgen
        for _ in 0..4 {
            if url_to_doc.contains_key(&target) {
                break;
            }
            match alias.get(&target) {
                Some(&t) => target = t,
                None => break,
            }
        }
        if let Some(&id) = url_to_doc.get(&target) {
            url_to_doc.insert(a, id);
        }
    }
    drop(alias);
    log::info!(
        "{total} Records gelesen, {n} Dokumente ausgewählt ({:.1?})",
        t0.elapsed()
    );

    // Hosts: zusammenhängende Doc-ID-Bereiche; pro Dokument nur noch eine Host-ID.
    let mut hosts: Vec<(String, u32, u32)> = Vec::new();
    let mut doc_host: Vec<u32> = Vec::with_capacity(n);
    for (i, d) in docs.iter().enumerate() {
        let h = d.0.split('/').next().unwrap_or("");
        match hosts.last_mut() {
            Some(last) if last.0 == h => last.2 += 1,
            _ => hosts.push((h.to_string(), i as u32, 1)),
        }
        doc_host.push(hosts.len() as u32 - 1);
    }
    let locs: Vec<RecordLoc> = docs.into_iter().map(|d| d.2).collect();
    // Registrierbare Domain je Host (für intern/extern bei Links)
    let mut site_ids: FxHashMap<&str, u32> = FxHashMap::default();
    let host_site: Vec<u32> = hosts
        .iter()
        .map(|h| {
            let next = site_ids.len() as u32;
            *site_ids.entry(site_of_rev(&h.0)).or_insert(next)
        })
        .collect();
    drop(site_ids);

    // Temporäres Ausgabeverzeichnis
    let tmp_dir = cfg.index_dir.with_extension("tmp");
    if tmp_dir.exists() {
        fs::remove_dir_all(&tmp_dir)?;
    }
    fs::create_dir_all(&tmp_dir)?;
    write_hosts(&tmp_dir, &hosts)?;
    drop(hosts);

    let reader = StoreReader::open(&cfg.store_dir)?;

    // ---------- 3. Linkgraph & Ankertexte ----------
    let t1 = Instant::now();
    let mut graph = LinkGraph::new();
    let mut anchors: Vec<AnchorBuf> = (0..n).map(|_| Vec::new()).collect();
    let site_of_doc = |d: u32| host_site[doc_host[d as usize] as usize];
    for (ci, chunk) in locs.chunks(CHUNK).enumerate() {
        let base = (ci * CHUNK) as u32;
        let res: Vec<Result<(Vec<u32>, Vec<(u32, String)>)>> = chunk
            .par_iter()
            .enumerate()
            .map(|(j, loc)| {
                let src = base + j as u32;
                let rec = reader.read(*loc)?;
                let src_site = site_of_doc(src);
                let mut targets = Vec::new();
                let mut anc = Vec::new();
                for l in &rec.links {
                    let Some(&t) = url_to_doc.get(&url_hash(&l.url)) else {
                        continue;
                    };
                    if t == src {
                        continue;
                    }
                    let internal = site_of_doc(t) == src_site;
                    targets.push(if internal { t | INTERNAL_BIT } else { t });
                    let a = text::fold(l.anchor.trim());
                    if !a.is_empty() && a.len() <= 120 && !GENERIC_ANCHORS.contains(&a.as_str()) {
                        anc.push((t, l.anchor.trim().to_string()));
                    }
                }
                targets.sort_unstable_by_key(|t| t & !INTERNAL_BIT);
                targets.dedup_by_key(|t| *t & !INTERNAL_BIT);
                Ok((targets, anc))
            })
            .collect();
        for r in res {
            let (targets, anc) = r?;
            graph.push_node(&targets);
            for (t, a) in anc {
                anchor_add(&mut anchors[t as usize], &a);
            }
        }
    }
    drop(url_to_doc);
    let pr = pagerank::pagerank(&graph, 30, 0.85);
    log::info!(
        "Linkgraph: {} Kanten, PageRank berechnet ({:.1?})",
        graph.num_edges(),
        t1.elapsed()
    );
    drop(graph);

    // ---------- 4. Invertieren ----------
    let t2 = Instant::now();
    let runs_dir = tmp_dir.join("runs");
    fs::create_dir_all(&runs_dir)?;
    let nshards = rayon::current_num_threads().clamp(1, 16);
    let mut spimi = Spimi::new(runs_dir.clone(), cfg.mem_budget_bytes, nshards);
    let mut store_w = DocStoreWriter::create(&tmp_dir)?;
    let mut meta_w = BufWriter::new(File::create(tmp_dir.join("docmeta.bin"))?);
    let mut len_sum = [0f64; NUM_FIELDS];
    for (ci, chunk) in locs.chunks(CHUNK).enumerate() {
        let base = ci * CHUNK;
        let outs: Vec<Result<DocOut>> = chunk
            .par_iter()
            .enumerate()
            .map(|(j, loc)| {
                let id = base + j;
                let rec = reader.read(*loc)?;
                Ok(analyze_doc(rec, &anchors[id], pr[id], nshards))
            })
            .collect();
        let mut stored = Vec::with_capacity(chunk.len());
        let mut per_shard: Vec<Vec<(u32, Vec<TermEntry>, Vec<String>)>> = (0..nshards)
            .map(|_| Vec::with_capacity(chunk.len()))
            .collect();
        for (j, o) in outs.into_iter().enumerate() {
            let o = o?;
            let id = (base + j) as u32;
            meta_w.write_all(&o.meta.to_bytes())?;
            for f in 0..NUM_FIELDS {
                len_sum[f] += o.lens[f] as f64;
            }
            for (s, (terms, words)) in o.shards.into_iter().enumerate() {
                per_shard[s].push((id, terms, words));
            }
            stored.push(o.stored);
        }
        store_w.add_many(&stored)?;
        spimi.add_chunk(per_shard);
        spimi.maybe_flush()?;
        for a in anchors[base..base + chunk.len()].iter_mut() {
            *a = Vec::new();
        }
        if ci % 50 == 49 {
            log::info!("{} / {n} Dokumente analysiert", base + chunk.len());
        }
    }
    store_w.finish()?;
    meta_w.flush()?;
    drop(meta_w);
    spimi.flush()?;
    log::info!(
        "Analyse fertig, {} Läufe ({:.1?}); Merge …",
        spimi.runs.len(),
        t2.elapsed()
    );

    // ---------- 5. Merge ----------
    let t3 = Instant::now();
    let num_terms = merge_runs(&spimi.runs, &tmp_dir)?;
    fs::remove_dir_all(&runs_dir)?;
    let word_df = spimi.take_words();
    write_words(&tmp_dir, word_df, n)?;

    let mut avg_len = [0f32; NUM_FIELDS];
    for f in 0..NUM_FIELDS {
        avg_len[f] = ((len_sum[f] / n as f64) as f32).max(1.0);
    }
    let meta = IndexMeta {
        version: FORMAT_VERSION,
        num_docs: n as u32,
        num_terms,
        avg_len,
        built_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    fs::write(tmp_dir.join("meta.json"), serde_json::to_vec_pretty(&meta)?)?;

    // Atomar austauschen
    if cfg.index_dir.exists() {
        let old = cfg.index_dir.with_extension("old");
        if old.exists() {
            fs::remove_dir_all(&old)?;
        }
        fs::rename(&cfg.index_dir, &old)?;
        fs::rename(&tmp_dir, &cfg.index_dir)?;
        fs::remove_dir_all(&old)?;
    } else {
        fs::rename(&tmp_dir, &cfg.index_dir)?;
    }
    log::info!(
        "Merge fertig: {num_terms} Terme ({:.1?}); gesamt {:.1?}",
        t3.elapsed(),
        t0.elapsed()
    );
    Ok(meta)
}

/// Gleiche registrierbare Domain? (vereinfachte Heuristik: letzte zwei Labels,
/// bei kurzen zweitletzten Labels wie `co.uk` die letzten drei)
#[cfg(test)]
fn same_site(rev_a: &str, rev_b: &str) -> bool {
    site_of_rev(rev_a) == site_of_rev(rev_b)
}

pub fn site_of_rev(rev: &str) -> &str {
    let mut it = rev.match_indices('.');
    let Some((i1, _)) = it.next() else { return rev };
    let second_end = it.next().map(|(i, _)| i).unwrap_or(rev.len());
    let second = &rev[i1 + 1..second_end];
    if second.len() <= 3
        && matches!(
            &rev[..i1],
            "uk" | "au" | "jp" | "nz" | "br" | "za" | "in" | "kr" | "tr"
        )
    {
        let third_end = it.next().map(|(i, _)| i).unwrap_or(rev.len());
        &rev[..third_end]
    } else {
        &rev[..second_end]
    }
}

/// Wörterbuch der Oberflächenwörter mit Dokumentfrequenz (für „Meinten Sie“ und
/// Autovervollständigung). Seltene Wörter werden bei großen Indizes verworfen.
fn write_words(dir: &Path, word_df: Vec<(String, u32)>, num_docs: usize) -> Result<()> {
    let min_df = if num_docs < 2_000 {
        1
    } else if num_docs < 200_000 {
        2
    } else {
        3
    };
    let mut words: Vec<(String, u32)> = word_df.into_iter().filter(|(_, d)| *d >= min_df).collect();
    words.par_sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut b = fst::MapBuilder::new(BufWriter::new(File::create(dir.join("words.fst"))?))?;
    for (w, d) in words {
        b.insert(w.as_bytes(), d as u64)?;
    }
    b.finish()?;
    Ok(())
}

fn write_hosts(dir: &Path, hosts: &[(String, u32, u32)]) -> Result<()> {
    let mut out = Vec::new();
    write_varint(&mut out, hosts.len() as u64);
    for (h, start, cnt) in hosts {
        write_str(&mut out, h);
        write_varint(&mut out, *start as u64);
        write_varint(&mut out, *cnt as u64);
    }
    fs::write(dir.join("hosts.bin"), out)?;
    Ok(())
}

#[inline]
fn shard_of(term: &str, nshards: usize) -> usize {
    (xxh3_64(term.as_bytes()) % nshards as u64) as usize
}

fn analyze_doc(rec: store::PageRecord, anchors: &[u8], pr: f32, nshards: usize) -> DocOut {
    let lang = Lang::from_u8(rec.lang);
    let mut map: HashMap<String, ([u32; NUM_FIELDS], Vec<u32>)> = HashMap::new();
    let mut lens = [0u32; NUM_FIELDS];

    let mut words: std::collections::HashSet<String> = std::collections::HashSet::new();
    let add_word = |t: &str, words: &mut std::collections::HashSet<String>| {
        if t.len() >= 2
            && t.len() <= 32
            && words.len() < 5000
            && !t.chars().all(|c| c.is_ascii_digit())
        {
            words.insert(text::fold(t));
        }
    };
    let mut pos = 0u32;
    text::tokenize(&rec.title, |t, _, _| {
        add_word(t, &mut words);
        let e = map.entry(text::index_term(t, lang)).or_default();
        e.0[F_TITLE] += 1;
        if (e.1.len() as u32) < MAX_POSITIONS {
            e.1.push(pos);
        }
        pos += 1;
    });
    lens[F_TITLE] = pos;
    let mut bpos = pos + BODY_POS_GAP;
    let body_start = bpos;
    for part in [rec.description.as_str(), rec.body.as_str()] {
        text::tokenize(part, |t, _, _| {
            add_word(t, &mut words);
            let e = map.entry(text::index_term(t, lang)).or_default();
            e.0[F_BODY] += 1;
            if (e.1.len() as u32) < MAX_POSITIONS {
                e.1.push(bpos);
            }
            bpos += 1;
        });
        bpos += BODY_POS_GAP;
    }
    lens[F_BODY] = bpos - body_start - 2 * BODY_POS_GAP;
    let mut add_tf = |text: &str, field: usize, w: u32, lens: &mut [u32; NUM_FIELDS]| {
        text::tokenize(text, |t, _, _| {
            map.entry(text::index_term(t, lang)).or_default().0[field] += w;
            lens[field] += w;
        });
    };
    add_tf(&rec.headings, F_HEADINGS, 1, &mut lens);
    let parsed = Url::parse(&rec.url).ok();
    if let Some(u) = &parsed {
        let host = urlnorm::host_key(u);
        let path = u.path();
        add_tf(&host, F_URL, 1, &mut lens);
        add_tf(path, F_URL, 1, &mut lens);
    }
    for (cnt, txt) in anchor_iter(anchors) {
        add_tf(txt, F_ANCHOR, cnt.min(10), &mut lens);
    }

    let (depth, home) = parsed
        .as_ref()
        .map(|u| (urlnorm::depth(u), u.path() == "/" && u.query().is_none()))
        .unwrap_or((0, false));
    let mut len_q = [0u8; NUM_FIELDS];
    for f in 0..NUM_FIELDS {
        len_q[f] = quantize_len(lens[f]);
    }
    let meta = DocMeta {
        len_q,
        quality: pagerank::quantize(pr),
        lang: lang as u8,
        flags: (depth.min(15) as u8) | if home { 0x10 } else { 0 },
    };
    let stored = StoredDoc {
        url: rec.url,
        title: rec.title,
        description: rec.description,
        body: prefix(&rec.body, STORED_BODY_BYTES).to_string(),
    };
    let mut shards: Vec<(Vec<TermEntry>, Vec<String>)> =
        (0..nshards).map(|_| Default::default()).collect();
    for (k, (tf, p)) in map {
        shards[shard_of(&k, nshards)].0.push((k, tf, p));
    }
    for w in words {
        shards[shard_of(&w, nshards)].1.push(w);
    }
    DocOut {
        shards,
        lens,
        meta,
        stored,
    }
}

// ---------------- SPIMI ----------------

struct TermBuf {
    last: u32,
    n: u32,
    data: Vec<u8>,
}

/// Ein Shard der Invertierung: verantwortlich für alle Terme mit `hash % n == i`.
/// Shards arbeiten parallel und unabhängig.
#[derive(Default)]
struct Shard {
    map: FxHashMap<String, TermBuf>,
    words: FxHashMap<String, u32>,
    bytes: usize,
}

impl Shard {
    fn add(&mut self, doc: u32, term: String, tf: &[u32; NUM_FIELDS], pos: &[u32]) {
        let klen = term.len();
        let bytes = &mut self.bytes;
        let e = self.map.entry(term).or_insert_with(|| {
            *bytes += klen + 72;
            TermBuf {
                last: DOC_END,
                n: 0,
                data: Vec::new(),
            }
        });
        let before = e.data.capacity();
        write_varint(&mut e.data, doc.wrapping_sub(e.last).wrapping_sub(1) as u64);
        for &t in tf {
            write_varint(&mut e.data, t as u64);
        }
        let cnt = pos_count(tf) as usize;
        let mut last = 0;
        for (j, &p) in pos.iter().take(cnt).enumerate() {
            write_varint(&mut e.data, (if j == 0 { p } else { p - last }) as u64);
            last = p;
        }
        for _ in pos.len().min(cnt)..cnt {
            write_varint(&mut e.data, 0);
        }
        e.last = doc;
        e.n += 1;
        self.bytes += e.data.capacity() - before;
    }

    fn add_word(&mut self, w: String) {
        let klen = w.len();
        let bytes = &mut self.bytes;
        *self.words.entry(w).or_insert_with(|| {
            *bytes += klen + 48;
            0
        }) += 1;
    }

    fn write_run(&mut self, path: &Path) -> Result<()> {
        let mut w = BufWriter::with_capacity(1 << 20, File::create(path)?);
        let mut terms: Vec<(String, TermBuf)> = std::mem::take(&mut self.map).into_iter().collect();
        terms.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let mut hdr = Vec::with_capacity(64);
        for (t, b) in terms {
            hdr.clear();
            write_str(&mut hdr, &t);
            write_varint(&mut hdr, b.n as u64);
            write_varint(&mut hdr, b.data.len() as u64);
            w.write_all(&hdr)?;
            w.write_all(&b.data)?;
        }
        w.flush()?;
        // Wörter bleiben im Speicher (klein), nur ihr Anteil am Budget wird neu gezählt
        self.bytes = self.words.keys().map(|k| k.len() + 48).sum();
        Ok(())
    }
}

struct Spimi {
    shards: Vec<Shard>,
    budget: usize,
    dir: PathBuf,
    /// Läufe in Schreibreihenfolge: Runde für Runde, innerhalb einer Runde nach Shard.
    /// Für jeden Term (genau ein Shard) sind die Läufe damit nach Doc-ID geordnet.
    runs: Vec<PathBuf>,
    round: usize,
}

impl Spimi {
    fn new(dir: PathBuf, budget: usize, nshards: usize) -> Spimi {
        Spimi {
            shards: (0..nshards).map(|_| Shard::default()).collect(),
            budget: budget.max(1 << 20),
            dir,
            runs: Vec::new(),
            round: 0,
        }
    }

    /// Fügt einen Block von Dokumenten (aufsteigende Doc-IDs) parallel über alle Shards ein.
    fn add_chunk(&mut self, per_shard: Vec<Vec<(u32, Vec<TermEntry>, Vec<String>)>>) {
        self.shards
            .par_iter_mut()
            .zip(per_shard.into_par_iter())
            .for_each(|(sh, items)| {
                for (id, terms, words) in items {
                    for (term, tf, pos) in terms {
                        sh.add(id, term, &tf, &pos);
                    }
                    for w in words {
                        sh.add_word(w);
                    }
                }
            });
    }

    fn bytes(&self) -> usize {
        self.shards.iter().map(|s| s.bytes).sum()
    }

    fn maybe_flush(&mut self) -> Result<()> {
        if self.bytes() >= self.budget {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        let round = self.round;
        let dir = self.dir.clone();
        let paths: Vec<Option<PathBuf>> = self
            .shards
            .par_iter_mut()
            .enumerate()
            .map(|(i, sh)| -> Result<Option<PathBuf>> {
                if sh.map.is_empty() {
                    return Ok(None);
                }
                let path = dir.join(format!("run-{round:05}-{i:02}.bin"));
                sh.write_run(&path)?;
                Ok(Some(path))
            })
            .collect::<Result<_>>()?;
        log::debug!(
            "Runde {round}: {} Läufe geschrieben",
            paths.iter().flatten().count()
        );
        self.runs.extend(paths.into_iter().flatten());
        self.round += 1;
        Ok(())
    }

    fn take_words(&mut self) -> Vec<(String, u32)> {
        let mut v = Vec::new();
        for sh in &mut self.shards {
            v.extend(std::mem::take(&mut sh.words));
        }
        v
    }
}

struct RunReader {
    r: BufReader<File>,
    cur: Option<(String, u32, Vec<u8>)>,
}

fn read_varint_io<R: Read>(r: &mut R) -> std::io::Result<Option<u64>> {
    let mut v = 0u64;
    let mut shift = 0;
    let mut b = [0u8; 1];
    loop {
        if r.read(&mut b)? == 0 {
            return Ok(None);
        }
        v |= ((b[0] & 0x7f) as u64) << shift;
        if b[0] < 0x80 {
            return Ok(Some(v));
        }
        shift += 7;
    }
}

impl RunReader {
    fn open(p: &Path) -> Result<RunReader> {
        let mut r = RunReader {
            r: BufReader::with_capacity(64 << 10, File::open(p)?),
            cur: None,
        };
        r.advance()?;
        Ok(r)
    }

    fn advance(&mut self) -> Result<()> {
        let Some(tlen) = read_varint_io(&mut self.r)? else {
            self.cur = None;
            return Ok(());
        };
        let mut t = vec![0u8; tlen as usize];
        self.r.read_exact(&mut t)?;
        let n = read_varint_io(&mut self.r)?.context("Lauf beschädigt")? as u32;
        let len = read_varint_io(&mut self.r)?.context("Lauf beschädigt")? as usize;
        let mut data = vec![0u8; len];
        self.r.read_exact(&mut data)?;
        self.cur = Some((String::from_utf8(t)?, n, data));
        Ok(())
    }
}

fn decode_run_chunk(data: &[u8], n: u32, enc: &mut ListEncoder) {
    let mut p = 0;
    let mut doc = DOC_END;
    for _ in 0..n {
        doc = doc
            .wrapping_add(read_varint(data, &mut p) as u32)
            .wrapping_add(1);
        let mut tf = [0u32; NUM_FIELDS];
        for t in tf.iter_mut() {
            *t = read_varint(data, &mut p) as u32;
        }
        let cnt = pos_count(&tf) as usize;
        let mut positions = Vec::with_capacity(cnt);
        let mut last = 0u32;
        for j in 0..cnt {
            let v = read_varint(data, &mut p) as u32;
            last = if j == 0 { v } else { last + v };
            positions.push(last);
        }
        enc.push(Posting { doc, tf, positions });
    }
}

fn merge_runs(runs: &[PathBuf], out_dir: &Path) -> Result<u64> {
    let mut readers: Vec<RunReader> = runs
        .iter()
        .map(|p| RunReader::open(p))
        .collect::<Result<_>>()?;
    let mut heap: BinaryHeap<Reverse<(String, usize)>> = BinaryHeap::new();
    for (i, r) in readers.iter().enumerate() {
        if let Some(c) = &r.cur {
            heap.push(Reverse((c.0.clone(), i)));
        }
    }
    let mut postings =
        BufWriter::with_capacity(1 << 20, File::create(out_dir.join("postings.bin"))?);
    let mut fst_b = fst::MapBuilder::new(BufWriter::new(File::create(out_dir.join("terms.fst"))?))?;
    let mut offset = 0u64;
    let mut buf = Vec::new();
    let mut num_terms = 0u64;
    while let Some(Reverse((term, _))) = heap.peek().cloned() {
        // Alle Läufe mit diesem Term, in Laufreihenfolge (= aufsteigende Doc-IDs)
        let mut idxs = Vec::new();
        while let Some(Reverse((t, i))) = heap.peek() {
            if *t != term {
                break;
            }
            idxs.push(*i);
            heap.pop();
        }
        idxs.sort_unstable();
        let mut enc = ListEncoder::new();
        for &i in &idxs {
            let (_, n, data) = readers[i].cur.take().unwrap();
            decode_run_chunk(&data, n, &mut enc);
            readers[i].advance()?;
            if let Some(c) = &readers[i].cur {
                heap.push(Reverse((c.0.clone(), i)));
            }
        }
        buf.clear();
        enc.finish(&mut buf);
        fst_b.insert(term.as_bytes(), offset)?;
        postings.write_all(&buf)?;
        offset += buf.len() as u64;
        num_terms += 1;
    }
    // Puffer, damit Lesezugriffe am Dateiende nie außerhalb liegen
    postings.write_all(&[0u8; 16])?;
    postings.flush()?;
    fst_b.finish()?;
    Ok(num_terms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_detection() {
        assert_eq!(site_of_rev("com.example.www"), "com.example");
        assert_eq!(site_of_rev("uk.co.bbc.www"), "uk.co.bbc");
        assert_eq!(site_of_rev("de"), "de");
        assert!(same_site("de.spiegel.www", "de.spiegel"));
        assert!(!same_site("de.spiegel", "de.zeit"));
    }
}
