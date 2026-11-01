//! Snippet-Erzeugung: wählt das Textfenster mit den meisten verschiedenen
//! Suchbegriffen und hebt Treffer hervor.

use crate::text::{self, Lang};
use std::collections::HashMap;

pub fn html_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '&' => o.push_str("&amp;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

const WINDOW: usize = 32;
const MAX_SNIPPET_BYTES: usize = 300;

/// Erzeugt ein HTML-Snippet. `terms` bildet Indexterme auf Gruppen-IDs ab.
pub fn make_snippet(
    body: &str,
    description: &str,
    lang: Lang,
    terms: &HashMap<String, usize>,
) -> String {
    let mut toks: Vec<(usize, usize, Option<usize>)> = Vec::new();
    text::tokenize(body, |t, s, e| {
        let g = terms
            .get(&text::index_term(t, lang))
            .or_else(|| terms.get(&text::fold(t)))
            .copied();
        toks.push((s, e, g));
    });
    let mut best: Option<(usize, usize, usize)> = None; // (start, distinct, hits)
    if toks.iter().any(|t| t.2.is_some()) {
        let mut counts: HashMap<usize, usize> = HashMap::new();
        let mut hits = 0usize;
        for i in 0..toks.len() {
            if let Some(g) = toks[i].2 {
                *counts.entry(g).or_insert(0) += 1;
                hits += 1;
            }
            if i >= WINDOW {
                if let Some(g) = toks[i - WINDOW].2 {
                    let c = counts.get_mut(&g).unwrap();
                    *c -= 1;
                    if *c == 0 {
                        counts.remove(&g);
                    }
                    hits -= 1;
                }
            }
            let start = i.saturating_sub(WINDOW - 1);
            let cand = (start, counts.len(), hits);
            if best.is_none_or(|b| (cand.1, cand.2) > (b.1, b.2)) {
                best = Some(cand);
            }
        }
    }
    match best {
        Some((start, _, _)) => {
            // Fenster etwas vor den ersten Treffer schieben, damit Kontext sichtbar ist
            let first_hit = (start..toks.len().min(start + WINDOW))
                .find(|&i| toks[i].2.is_some())
                .unwrap_or(start);
            let s = first_hit.saturating_sub(6);
            let e = (s + WINDOW).min(toks.len());
            render(body, &toks[s..e], s > 0, e < toks.len())
        }
        None if !description.is_empty() => html_escape(&truncate(description, 240)),
        None => {
            let e = toks.len().min(WINDOW);
            render(body, &toks[..e], false, e < toks.len())
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut o: String = s.chars().take(max).collect();
    if let Some(i) = o.rfind(' ') {
        o.truncate(i);
    }
    o.push_str(" …");
    o
}

fn render(body: &str, toks: &[(usize, usize, Option<usize>)], pre: bool, post: bool) -> String {
    if toks.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    if pre {
        out.push_str("… ");
    }
    let mut last = toks[0].0;
    let mut post = post;
    for (i, &(s, e, g)) in toks.iter().enumerate() {
        // Snippets auf ca. 300 Zeichen Quelltext begrenzen (z.B. bei langen Codeblöcken)
        if i > 0 && e - toks[0].0 > MAX_SNIPPET_BYTES {
            post = true;
            break;
        }
        out.push_str(&html_escape(&body[last..s]));
        if g.is_some() {
            out.push_str("<b>");
            out.push_str(&html_escape(&body[s..e]));
            out.push_str("</b>");
        } else {
            out.push_str(&html_escape(&body[s..e]));
        }
        last = e;
    }
    // Satzzeichen direkt nach dem letzten Token mitnehmen
    let tail: String = body[last..]
        .chars()
        .take_while(|c| !c.is_alphanumeric() && !c.is_whitespace())
        .collect();
    out.push_str(&html_escape(&tail));
    if post {
        out.push_str(" …");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights() {
        let mut terms = HashMap::new();
        terms.insert(text::index_term("crawler", Lang::En), 0);
        terms.insert(text::index_term("index", Lang::En), 1);
        let body = format!(
            "{} A fast crawler builds an index <quickly>. {}",
            "filler ".repeat(50),
            "end ".repeat(50)
        );
        let s = make_snippet(&body, "", Lang::En, &terms);
        assert!(s.contains("<b>crawler</b>"), "{s}");
        assert!(s.contains("<b>index</b>"));
        assert!(s.contains("&lt;quickly&gt;"));
        assert!(s.starts_with("… "));
        assert_eq!(
            make_snippet("nothing here", "Desc", Lang::En, &terms),
            "Desc"
        );
    }
}
