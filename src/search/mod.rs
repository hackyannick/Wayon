//! Suche: Dokument-für-Dokument-Auswertung über Postinglisten mit Skipping,
//! BM25F-Relevanz über fünf Felder, Termnähe, Phrasen, Titel- und Domain-Treffer,
//! PageRank als statisches Qualitätssignal und Domain-Diversität im Ergebnis.

pub mod query;
pub mod snippet;

use crate::index::pagerank::quality_score;
use crate::index::postings::{Cursor, DOC_END};
use crate::index::*;
use crate::text::{self, Lang};
use serde::Serialize;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::time::Instant;

// ---- Ranking-Parameter ----
const K1: f32 = 1.2;
const FIELD_W: [f32; NUM_FIELDS] = [2.5, 1.4, 1.0, 1.6, 2.2];
const FIELD_B: [f32; NUM_FIELDS] = [0.4, 0.5, 0.75, 0.3, 0.6];
const STOPWORD_W: f32 = 0.25;
const PROX_W: f32 = 3.0;
const TITLE_ALL_W: f32 = 2.0;
const HOST_W: f32 = 3.0;
const HOME_W: f32 = 1.0;
const QUALITY_W: f32 = 3.0;
const DEPTH_PENALTY: f32 = 0.12;
const RERANK_POOL: usize = 300;
const MAX_PER_HOST: usize = 2;
const MAX_CANDIDATES: u64 = 5_000_000;

#[derive(Clone, Debug, Serialize)]
pub struct Hit {
    pub doc: u32,
    pub score: f32,
    pub url: String,
    pub title: String,
    pub snippet: String,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct SearchResults {
    pub query: String,
    /// Falls automatisch korrigiert wurde: die tatsächlich gesuchte Query.
    pub corrected: Option<String>,
    /// Vorschlag „Meinten Sie …“.
    pub suggestion: Option<String>,
    pub total: u64,
    pub offset: usize,
    pub hits: Vec<Hit>,
    pub took_ms: f64,
}

#[derive(Clone, Copy)]
struct Score(f32);
impl PartialEq for Score {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o).is_eq()
    }
}
impl Eq for Score {}
impl PartialOrd for Score {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Score {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&o.0)
    }
}

/// Ein Query-Begriff mit allen Indextermen (ungestemmt / deutsch / englisch).
struct Group<'a> {
    cursors: Vec<Cursor<'a>>,
    idf: f32,
    weight: f32,
    doc: u32,
}

impl<'a> Group<'a> {
    fn new(index: &'a Index, raw: &str, weight: f32) -> Group<'a> {
        let variants = text::query_variants(raw);
        let cursors: Vec<Cursor> = variants.iter().filter_map(|v| index.cursor(v)).collect();
        let n = index.num_docs() as f32;
        let df = (cursors.iter().map(|c| c.df as f32).sum::<f32>()).min(n);
        let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
        let mut g = Group {
            cursors,
            idf,
            weight,
            doc: DOC_END,
        };
        g.refresh();
        g
    }

    fn refresh(&mut self) {
        self.doc = self
            .cursors
            .iter()
            .map(|c| c.doc())
            .min()
            .unwrap_or(DOC_END);
    }

    fn df(&self) -> u32 {
        self.cursors.iter().map(|c| c.df).sum()
    }

    fn advance(&mut self, t: u32) -> u32 {
        if self.doc >= t {
            return self.doc;
        }
        for c in &mut self.cursors {
            if c.doc() < t {
                c.advance(t);
            }
        }
        self.refresh();
        self.doc
    }

    fn tfs(&self) -> [u32; NUM_FIELDS] {
        let mut t = [0; NUM_FIELDS];
        for c in self.cursors.iter().filter(|c| c.doc() == self.doc) {
            for (f, v) in c.tfs().iter().enumerate() {
                t[f] += v;
            }
        }
        t
    }

    fn positions(&mut self, out: &mut Vec<u32>, tmp: &mut Vec<u32>) {
        out.clear();
        let d = self.doc;
        for c in self.cursors.iter_mut().filter(|c| c.doc() == d) {
            c.positions(tmp);
            out.extend_from_slice(tmp);
        }
        out.sort_unstable();
        out.dedup();
    }
}

fn bm25f(index: &Index, tfs: &[u32; NUM_FIELDS], meta: &DocMeta, idf: f32) -> f32 {
    let mut t = 0.0;
    for f in 0..NUM_FIELDS {
        if tfs[f] > 0 {
            let len = index.field_len(meta.len_q[f]);
            let norm = 1.0 - FIELD_B[f] + FIELD_B[f] * len / index.meta.avg_len[f];
            t += FIELD_W[f] * tfs[f] as f32 / norm.max(0.1);
        }
    }
    idf * t * (K1 + 1.0) / (K1 + t)
}

/// Kleinstes Fenster, das aus jeder Liste eine Position enthält.
fn min_window(lists: &[&[u32]]) -> u32 {
    let k = lists.len();
    let mut idx = vec![0usize; k];
    let mut best = u32::MAX;
    loop {
        let (mut lo, mut lo_i, mut hi) = (u32::MAX, 0, 0);
        for (i, l) in lists.iter().enumerate() {
            let v = l[idx[i]];
            if v < lo {
                lo = v;
                lo_i = i;
            }
            hi = hi.max(v);
        }
        best = best.min(hi - lo + 1);
        idx[lo_i] += 1;
        if idx[lo_i] >= lists[lo_i].len() {
            return best;
        }
    }
}

fn phrase_match(lists: &[&[u32]]) -> bool {
    let Some(first) = lists.first() else {
        return true;
    };
    first.iter().any(|&p| {
        lists
            .iter()
            .enumerate()
            .skip(1)
            .all(|(i, l)| l.binary_search(&(p + i as u32)).is_ok())
    })
}

pub struct Searcher<'a> {
    pub index: &'a Index,
}

struct Plan {
    group_words: Vec<String>,
    required: Vec<usize>,
    phrases: Vec<Vec<usize>>,
}

impl<'a> Searcher<'a> {
    pub fn new(index: &'a Index) -> Searcher<'a> {
        Searcher { index }
    }

    pub fn search(&self, q: &str, offset: usize, limit: usize) -> SearchResults {
        let t0 = Instant::now();
        let mut res = self.search_inner(q, offset, limit);
        if res.total == 0 {
            if let Some(s) = res.suggestion.clone() {
                let mut r2 = self.search_inner(&s, offset, limit);
                if r2.total > 0 {
                    r2.corrected = Some(s);
                    r2.suggestion = None;
                    r2.query = q.to_string();
                    res = r2;
                }
            }
        }
        res.took_ms = t0.elapsed().as_secs_f64() * 1000.0;
        res
    }

    fn plan(&self, pq: &query::ParsedQuery) -> Plan {
        let mut group_words: Vec<String> = Vec::new();
        let mut word_group = Vec::new();
        for w in &pq.words {
            let f = text::fold(w);
            let gi = match group_words.iter().position(|g| text::fold(g) == f) {
                Some(i) => i,
                None => {
                    group_words.push(w.clone());
                    group_words.len() - 1
                }
            };
            word_group.push(gi);
        }
        let phrases: Vec<Vec<usize>> = pq
            .phrases
            .iter()
            .map(|p| p.iter().map(|&i| word_group[i]).collect())
            .collect();
        let in_phrase = |g: usize| phrases.iter().any(|p| p.contains(&g));
        let mut required: Vec<usize> = (0..group_words.len())
            .filter(|&g| in_phrase(g) || !text::is_stopword(&text::fold(&group_words[g])))
            .collect();
        if required.is_empty() {
            required = (0..group_words.len()).collect();
        }
        Plan {
            group_words,
            required,
            phrases,
        }
    }

    fn search_inner(&self, q: &str, offset: usize, limit: usize) -> SearchResults {
        let index = self.index;
        let pq = query::parse(q);
        let mut res = SearchResults {
            query: q.to_string(),
            offset,
            ..Default::default()
        };
        if pq.is_empty() || index.num_docs() == 0 {
            return res;
        }
        let plan = self.plan(&pq);
        res.suggestion = self.suggest(q, &pq);

        let mut include: Vec<(u32, u32)> = Vec::new();
        for s in &pq.sites {
            include.extend(index.site_ranges(s));
        }
        include.sort_unstable();
        if !pq.sites.is_empty() && include.is_empty() {
            return res;
        }
        let mut exclude: Vec<(u32, u32)> = pq
            .not_sites
            .iter()
            .flat_map(|s| index.site_ranges(s))
            .collect();
        exclude.sort_unstable();

        let pool = RERANK_POOL.max(offset + limit);
        let (mut top, total) =
            self.evaluate(&plan, &pq, &include, &exclude, plan.required.len(), pool);
        let n_req = plan.required.len();
        if total < 5 && n_req > 1 {
            // Gelockerte Suche: ein Begriff darf fehlen
            let (top2, total2) = self.evaluate(&plan, &pq, &include, &exclude, n_req - 1, pool);
            if total2 > total {
                let seen: std::collections::HashSet<u32> = top.iter().map(|t| t.1).collect();
                // Vollständige Treffer zuerst
                let best_full = top.iter().map(|t| t.0).fold(f32::MIN, f32::max);
                for (s, d) in top2 {
                    if !seen.contains(&d) {
                        top.push((s.min(best_full - 0.001), d));
                    }
                }
                res.total = total2;
            } else {
                res.total = total;
            }
        } else {
            res.total = total;
        }
        top.sort_by(|a, b| b.0.total_cmp(&a.0));

        // Domain-Diversität: höchstens MAX_PER_HOST Treffer pro Host am Stück
        let mut per_host: HashMap<&str, usize> = HashMap::new();
        let mut ordered = Vec::with_capacity(top.len());
        let mut deferred = Vec::new();
        for (s, d) in top {
            let h = index.host_of(d);
            let c = per_host.entry(h).or_insert(0);
            *c += 1;
            if *c <= MAX_PER_HOST {
                ordered.push((s, d));
            } else {
                deferred.push((s, d));
            }
        }
        ordered.extend(deferred);

        let mut terms: HashMap<String, usize> = HashMap::new();
        for (g, w) in plan.group_words.iter().enumerate() {
            for v in text::query_variants(w) {
                terms.entry(v).or_insert(g);
            }
        }
        for (s, d) in ordered.into_iter().skip(offset).take(limit) {
            let Ok(doc) = index.docs.get(d) else { continue };
            let lang = Lang::from_u8(index.doc_meta(d).lang);
            let snippet = snippet::make_snippet(&doc.body, &doc.description, lang, &terms);
            let title = if doc.title.is_empty() {
                doc.url.clone()
            } else {
                doc.title
            };
            res.hits.push(Hit {
                doc: d,
                score: s,
                url: doc.url,
                title,
                snippet,
            });
        }
        res
    }

    /// Wertet die Query aus; liefert die besten `pool` (Score, Doc) und die Trefferzahl.
    fn evaluate(
        &self,
        plan: &Plan,
        pq: &query::ParsedQuery,
        include: &[(u32, u32)],
        exclude: &[(u32, u32)],
        min_required: usize,
        pool: usize,
    ) -> (Vec<(f32, u32)>, u64) {
        let index = self.index;
        let mut groups: Vec<Group> = plan
            .group_words
            .iter()
            .enumerate()
            .map(|(gi, w)| {
                let w8 = if plan.required.contains(&gi) {
                    1.0
                } else {
                    STOPWORD_W
                };
                Group::new(index, w, w8)
            })
            .collect();
        let mut excluded: Vec<Group> = pq
            .excluded
            .iter()
            .map(|w| Group::new(index, w, 0.0))
            .collect();
        let required = &plan.required;
        let strict = min_required >= required.len();
        if strict && required.iter().any(|&g| groups[g].cursors.is_empty()) {
            return (Vec::new(), 0);
        }
        // Reihenfolge nach Seltenheit: seltenster Begriff treibt die Schnittmenge
        let mut req_sorted = required.clone();
        req_sorted.sort_by_key(|&g| groups[g].df());
        let n_groups = groups.len();
        let host_words: Vec<String> = plan
            .group_words
            .iter()
            .map(|w| text::fold(w))
            .filter(|w| w.len() >= 3)
            .collect();

        let mut heap: BinaryHeap<Reverse<(Score, u32)>> = BinaryHeap::new();
        let mut total = 0u64;
        let mut pos_lists: Vec<Vec<u32>> = vec![Vec::new(); n_groups];
        let mut present = vec![false; n_groups];
        let mut tmp = Vec::new();
        let mut inc_i = 0usize;
        let mut host_cache: (&str, usize) = ("", 0);
        // Stufe 1 bewertet alle Kandidaten mit billigen Signalen,
        // Stufe 2 berechnet die Termnähe nur für die besten `first_pool`.
        let first_pool = (pool * 4).max(1000);
        let mut doc = 0u32;

        loop {
            // ---- nächsten Kandidaten finden ----
            if strict {
                let lead = req_sorted[0];
                doc = groups[lead].advance(doc);
                if doc == DOC_END {
                    break;
                }
                let mut aligned = true;
                for &g in &req_sorted[1..] {
                    let d = groups[g].advance(doc);
                    if d > doc {
                        doc = d;
                        aligned = false;
                        break;
                    }
                }
                if !aligned {
                    continue;
                }
            } else {
                // mindestens `min_required` der Pflichtbegriffe
                for &g in required {
                    groups[g].advance(doc);
                }
                let d = required
                    .iter()
                    .map(|&g| groups[g].doc)
                    .min()
                    .unwrap_or(DOC_END);
                if d == DOC_END {
                    break;
                }
                doc = d;
                let cnt = required.iter().filter(|&&g| groups[g].doc == doc).count();
                if cnt < min_required.max(1) {
                    doc += 1;
                    continue;
                }
            }

            // ---- Filter ----
            if !include.is_empty() {
                while inc_i < include.len() && include[inc_i].1 <= doc {
                    inc_i += 1;
                }
                if inc_i >= include.len() {
                    break;
                }
                if doc < include[inc_i].0 {
                    doc = include[inc_i].0;
                    continue;
                }
            }
            let next = doc.saturating_add(1);
            if exclude.iter().any(|r| doc >= r.0 && doc < r.1)
                || excluded.iter_mut().any(|g| g.advance(doc) == doc)
            {
                doc = next;
                continue;
            }
            for (gi, g) in groups.iter_mut().enumerate() {
                present[gi] = g.advance(doc) == doc;
            }
            // Phrasen prüfen
            let mut ok = true;
            for p in &plan.phrases {
                for &g in p {
                    if present[g] {
                        let (a, b) = (&mut groups[g], &mut pos_lists[g]);
                        a.positions(b, &mut tmp);
                    }
                }
                if !p.iter().all(|&g| present[g])
                    || !phrase_match(
                        &p.iter()
                            .map(|&g| pos_lists[g].as_slice())
                            .collect::<Vec<_>>(),
                    )
                {
                    ok = false;
                    break;
                }
            }
            if !ok {
                doc = next;
                continue;
            }
            total += 1;
            if total > MAX_CANDIDATES {
                break;
            }

            // ---- Score ----
            let meta = index.doc_meta(doc);
            let mut s = 0.0;
            let mut in_title = 0;
            for (gi, g) in groups.iter().enumerate() {
                if present[gi] {
                    let tfs = g.tfs();
                    s += g.weight * bm25f(index, &tfs, &meta, g.idf);
                    if tfs[F_TITLE] > 0 {
                        in_title += 1;
                    }
                }
            }
            s += QUALITY_W * quality_score(meta.quality)
                - DEPTH_PENALTY * meta.depth().min(8) as f32;
            // Billige Zusatzsignale zuerst: Titel und Domain
            if in_title == n_groups {
                s += TITLE_ALL_W;
                // Titel besteht (fast) nur aus den Suchbegriffen
                if index.field_len(meta.len_q[F_TITLE]) <= n_groups as f32 * 1.5 + 1.0 {
                    s += 0.5 * TITLE_ALL_W;
                }
            }
            if !host_words.is_empty() {
                let host = index.host_of(doc);
                // Dokumente eines Hosts liegen hintereinander → Ergebnis zwischenspeichern
                if !std::ptr::eq(host, host_cache.0) {
                    let site = crate::index::builder::site_of_rev(host);
                    let matched = host_words
                        .iter()
                        .filter(|w| {
                            site.split('.')
                                .skip(1)
                                .flat_map(|l| l.split('-'))
                                .any(|l| l == w.as_str())
                        })
                        .count();
                    host_cache = (host, matched);
                }
                if host_cache.1 > 0 {
                    let frac = host_cache.1 as f32 / n_groups as f32;
                    s += HOST_W * frac;
                    if meta.is_home() {
                        s += HOME_W * frac;
                    }
                }
            }
            let threshold = if heap.len() >= first_pool {
                heap.peek().map(|r| r.0 .0 .0).unwrap_or(f32::MIN)
            } else {
                f32::MIN
            };
            if heap.len() < first_pool {
                heap.push(Reverse((Score(s), doc)));
            } else if s > threshold {
                heap.pop();
                heap.push(Reverse((Score(s), doc)));
            }
            doc = next;
            if doc == DOC_END {
                break;
            }
        }
        // ---- Stufe 2: Termnähe für die besten Kandidaten ----
        let mut cands: Vec<(f32, u32)> = heap.into_iter().map(|Reverse((s, d))| (s.0, d)).collect();
        let proximity_groups: Vec<usize> =
            (0..n_groups).filter(|&g| groups[g].weight >= 1.0).collect();
        if proximity_groups.len() >= 2 {
            cands.sort_unstable_by_key(|c| c.1);
            let mut fresh: Vec<Group> = proximity_groups
                .iter()
                .map(|&g| Group::new(index, &plan.group_words[g], 1.0))
                .collect();
            let k_total = n_groups as f32;
            for c in cands.iter_mut() {
                let mut lists: Vec<Vec<u32>> = Vec::with_capacity(fresh.len());
                for g in fresh.iter_mut() {
                    if g.advance(c.1) == c.1 {
                        let mut v = Vec::new();
                        g.positions(&mut v, &mut tmp);
                        if !v.is_empty() {
                            lists.push(v);
                        }
                    }
                }
                if lists.len() >= 2 {
                    let refs: Vec<&[u32]> = lists.iter().map(|v| v.as_slice()).collect();
                    let k = refs.len() as f32;
                    let w = min_window(&refs) as f32;
                    c.0 += PROX_W * (k / w.max(k)) * (k / k_total);
                }
            }
        }
        cands.sort_unstable_by(|a, b| b.0.total_cmp(&a.0));
        cands.truncate(pool);
        (cands, total)
    }

    /// „Meinten Sie“: seltene Wörter durch häufige, ähnliche Wörter ersetzen.
    fn suggest(&self, q: &str, pq: &query::ParsedQuery) -> Option<String> {
        let index = self.index;
        let mut changed = false;
        let mut out = q.to_string();
        for w in &pq.words {
            let f = text::fold(w);
            let len = f.chars().count();
            if len < 3 || f.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let own = index.word_df(&f);
            if own >= 3 {
                continue;
            }
            let dist = if len >= 8 { 2 } else { 1 };
            let mut cands = index.fuzzy_words(&f, dist);
            // Vertauschte Nachbarbuchstaben ("jsno" → "json") zählen als ein Fehler
            let chars: Vec<char> = f.chars().collect();
            for i in 0..chars.len().saturating_sub(1) {
                let mut c = chars.clone();
                c.swap(i, i + 1);
                let w: String = c.into_iter().collect();
                let df = index.word_df(&w);
                if df > 0 {
                    cands.push((w, df));
                }
            }
            let best = cands
                .into_iter()
                .filter(|(c, _)| *c != f)
                .max_by_key(|(c, d)| (*d, Reverse(c.len().abs_diff(len))));
            if let Some((cand, df)) = best {
                let min_df = if index.num_docs() < 1000 { 1 } else { 3 };
                if df >= min_df && df > own.saturating_mul(5) {
                    out = replace_word(&out, w, &cand);
                    changed = true;
                }
            }
        }
        changed.then_some(out)
    }

    /// Autovervollständigung für das letzte Wort der Eingabe.
    pub fn complete(&self, input: &str, limit: usize) -> Vec<String> {
        let trimmed = input.trim_start();
        if trimmed.is_empty() || input.ends_with(' ') {
            return Vec::new();
        }
        let (head, last) = match trimmed.rfind(' ') {
            Some(i) => (&trimmed[..=i], &trimmed[i + 1..]),
            None => ("", trimmed),
        };
        let f = text::fold(last);
        self.index
            .complete_word(&f, limit)
            .into_iter()
            .map(|(w, _)| format!("{}{}", head.to_lowercase(), w))
            .collect()
    }
}

fn replace_word(q: &str, from: &str, to: &str) -> String {
    let mut out = String::new();
    let mut done = false;
    let mut last = 0;
    text::tokenize(q, |t, s, e| {
        if !done && t == from {
            out.push_str(&q[last..s]);
            out.push_str(to);
            last = e;
            done = true;
        }
    });
    out.push_str(&q[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_and_phrases() {
        assert_eq!(min_window(&[&[1, 10, 20], &[12, 30], &[9]]), 4);
        assert!(phrase_match(&[&[3, 8], &[4, 20], &[5]]));
        assert!(!phrase_match(&[&[3, 8], &[5, 20]]));
        assert_eq!(replace_word("hallo welt", "welt", "wald"), "hallo wald");
    }
}
