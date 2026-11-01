//! Query-Parser: Wörter, "Phrasen", -Ausschluss, site:, -site:.

use crate::text;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParsedQuery {
    /// Positive Suchwörter (Rohform) in Reihenfolge.
    pub words: Vec<String>,
    /// Phrasen als Listen von Indizes in `words`.
    pub phrases: Vec<Vec<usize>>,
    pub excluded: Vec<String>,
    pub sites: Vec<String>,
    pub not_sites: Vec<String>,
}

impl ParsedQuery {
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }
}

pub fn parse(q: &str) -> ParsedQuery {
    let mut out = ParsedQuery::default();
    let chars: Vec<char> = q.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '"' || c == '„' || c == '“' || c == '”' {
            let end = chars[i + 1..]
                .iter()
                .position(|&c| c == '"' || c == '“' || c == '”')
                .map(|p| p + i + 1)
                .unwrap_or(chars.len());
            let phrase: String = chars[i + 1..end].iter().collect();
            let mut idxs = Vec::new();
            text::tokenize(&phrase, |t, _, _| {
                idxs.push(out.words.len());
                out.words.push(t.to_string());
            });
            if idxs.len() > 1 {
                out.phrases.push(idxs);
            }
            i = end + 1;
            continue;
        }
        let end = chars[i..]
            .iter()
            .position(|c| c.is_whitespace())
            .map(|p| p + i)
            .unwrap_or(chars.len());
        let tok: String = chars[i..end].iter().collect();
        i = end;
        let lower = tok.to_lowercase();
        if let Some(s) = lower.strip_prefix("site:") {
            if !s.is_empty() {
                out.sites.push(s.to_string());
            }
            continue;
        }
        if let Some(s) = lower.strip_prefix("-site:") {
            if !s.is_empty() {
                out.not_sites.push(s.to_string());
            }
            continue;
        }
        if let Some(rest) = tok.strip_prefix('-') {
            text::tokenize(rest, |t, _, _| out.excluded.push(t.to_string()));
            continue;
        }
        // Wörter mit Bindestrich/Punkt (z.B. "e-mail", "node.js") als Phrase behandeln
        let mut idxs = Vec::new();
        text::tokenize(&tok, |t, _, _| {
            idxs.push(out.words.len());
            out.words.push(t.to_string());
        });
        if idxs.len() > 1 {
            out.phrases.push(idxs);
        }
    }
    out.words.truncate(32);
    out.phrases.retain(|p| p.iter().all(|&i| i < 32));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_operators() {
        let q = parse(r#"rust "web crawler" -python site:example.com -site:spam.org e-mail"#);
        assert_eq!(q.words, vec!["rust", "web", "crawler", "e", "mail"]);
        assert_eq!(q.phrases, vec![vec![1, 2], vec![3, 4]]);
        assert_eq!(q.excluded, vec!["python"]);
        assert_eq!(q.sites, vec!["example.com"]);
        assert_eq!(q.not_sites, vec!["spam.org"]);
        let q = parse("„Hallo Welt“");
        assert_eq!(q.phrases.len(), 1);
        assert!(parse("   ").is_empty());
    }
}
