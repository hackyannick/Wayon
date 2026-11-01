//! Extraktion von Titel, Beschreibung, Überschriften, Fließtext und Links aus HTML.
//! Navigations-, Footer-, Script- und ähnliche Bereiche werden beim Fließtext
//! übersprungen (einfache Boilerplate-Entfernung).

use crate::store::Link;
use crate::urlnorm;
use scraper::{ElementRef, Html, Node, Selector};
use std::sync::OnceLock;
use url::Url;

pub const MAX_BODY_CHARS: usize = 200_000;
pub const MAX_LINKS: usize = 1000;

#[derive(Debug, Default)]
pub struct Extracted {
    pub title: String,
    pub description: String,
    pub headings: String,
    pub body: String,
    pub links: Vec<Link>,
    pub canonical: Option<String>,
    pub lang_hint: Option<String>,
    pub noindex: bool,
    pub nofollow: bool,
}

struct Sels {
    title: Selector,
    meta: Selector,
    link_canon: Selector,
    base: Selector,
    anchors: Selector,
    headings: Selector,
    body: Selector,
}

fn sels() -> &'static Sels {
    static S: OnceLock<Sels> = OnceLock::new();
    S.get_or_init(|| Sels {
        title: Selector::parse("title").unwrap(),
        meta: Selector::parse("meta").unwrap(),
        link_canon: Selector::parse("link[rel]").unwrap(),
        base: Selector::parse("base[href]").unwrap(),
        anchors: Selector::parse("a[href]").unwrap(),
        headings: Selector::parse("h1, h2, h3").unwrap(),
        body: Selector::parse("body").unwrap(),
    })
}

const SKIP_TAGS: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "nav", "footer", "aside", "form", "iframe",
    "select", "button", "canvas", "object", "embed", "head", "dialog", "menu",
];

const BLOCK_TAGS: &[&str] = &[
    "p",
    "div",
    "br",
    "li",
    "ul",
    "ol",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "tr",
    "td",
    "th",
    "table",
    "section",
    "article",
    "main",
    "header",
    "blockquote",
    "pre",
    "dd",
    "dt",
    "dl",
    "figcaption",
    "figure",
    "hr",
    "address",
    "details",
    "summary",
];

/// Normalisiert Whitespace auf einzelne Leerzeichen.
pub fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            space = !out.is_empty();
        } else {
            if space {
                out.push(' ');
                space = false;
            }
            out.push(c);
        }
    }
    out
}

fn text_of(e: ElementRef) -> String {
    collapse_ws(&e.text().collect::<Vec<_>>().join(" "))
}

fn is_boilerplate(e: &scraper::node::Element) -> bool {
    if SKIP_TAGS.contains(&e.name()) {
        return true;
    }
    if e.attr("hidden").is_some() || e.attr("aria-hidden") == Some("true") {
        return true;
    }
    if matches!(
        e.attr("role"),
        Some("navigation") | Some("banner") | Some("contentinfo") | Some("dialog")
    ) {
        return true;
    }
    let idc = format!(
        "{} {}",
        e.attr("id").unwrap_or(""),
        e.attr("class").unwrap_or("")
    )
    .to_ascii_lowercase();
    idc.contains("cookie")
        || idc.contains("consent")
        || idc.contains("breadcrumb")
        || idc.contains("sidebar")
}

fn truncate_chars(s: &mut String, max: usize) {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

pub fn extract(html: &str, page_url: &Url) -> Extracted {
    let doc = Html::parse_document(html);
    let s = sels();
    let mut ex = Extracted::default();

    if let Some(t) = doc.select(&s.title).next() {
        ex.title = text_of(t);
    }
    ex.lang_hint = doc.root_element().attr("lang").map(str::to_string);

    for m in doc.select(&s.meta) {
        let name = m
            .attr("name")
            .or_else(|| m.attr("property"))
            .unwrap_or("")
            .to_ascii_lowercase();
        let content = m.attr("content").unwrap_or("");
        match name.as_str() {
            "description" | "og:description" if ex.description.is_empty() => {
                ex.description = collapse_ws(content);
            }
            "og:title" if ex.title.is_empty() => ex.title = collapse_ws(content),
            "robots" | "wayonbot" => {
                let c = content.to_ascii_lowercase();
                ex.noindex |= c.contains("noindex") || c.contains("none");
                ex.nofollow |= c.contains("nofollow") || c.contains("none");
            }
            _ => {}
        }
        if ex.lang_hint.is_none() {
            if let Some(l) = m
                .attr("http-equiv")
                .filter(|h| h.eq_ignore_ascii_case("content-language"))
            {
                let _ = l;
                ex.lang_hint = Some(content.to_string());
            }
        }
    }

    let base = doc
        .select(&s.base)
        .next()
        .and_then(|b| b.attr("href"))
        .and_then(|h| page_url.join(h).ok())
        .unwrap_or_else(|| page_url.clone());

    for l in doc.select(&s.link_canon) {
        let rel = l.attr("rel").unwrap_or("").to_ascii_lowercase();
        if rel.split_whitespace().any(|r| r == "canonical") {
            if let Some(u) = l.attr("href").and_then(|h| urlnorm::resolve(&base, h)) {
                ex.canonical = Some(u.to_string());
            }
        }
    }

    let mut heads = Vec::new();
    for h in doc.select(&s.headings).take(64) {
        let t = text_of(h);
        if !t.is_empty() {
            heads.push(t);
        }
    }
    ex.headings = heads.join(" \u{2022} ");
    truncate_chars(&mut ex.headings, 4000);
    if ex.title.is_empty() {
        ex.title = heads.first().cloned().unwrap_or_default();
    }
    truncate_chars(&mut ex.title, 300);
    truncate_chars(&mut ex.description, 1000);

    // Fließtext per DFS mit Boilerplate-Filter.
    let mut raw = String::new();
    if let Some(body) = doc.select(&s.body).next() {
        enum Item<'a> {
            N(ego_tree::NodeRef<'a, Node>),
            Sep,
        }
        let mut stack = vec![Item::N(*body)];
        while let Some(it) = stack.pop() {
            if raw.len() > MAX_BODY_CHARS {
                break;
            }
            match it {
                Item::Sep => raw.push('\n'),
                Item::N(n) => match n.value() {
                    Node::Text(t) => raw.push_str(t),
                    Node::Element(e) => {
                        if is_boilerplate(e) {
                            continue;
                        }
                        let block = BLOCK_TAGS.contains(&e.name());
                        if block {
                            stack.push(Item::Sep);
                        }
                        if e.name() == "img" {
                            if let Some(alt) = e.attr("alt") {
                                raw.push(' ');
                                raw.push_str(alt);
                                raw.push(' ');
                            }
                        }
                        let children: Vec<_> = n.children().collect();
                        for c in children.into_iter().rev() {
                            stack.push(Item::N(c));
                        }
                        if block {
                            raw.push('\n');
                        }
                    }
                    _ => {}
                },
            }
        }
    }
    ex.body = collapse_ws(&raw);
    truncate_chars(&mut ex.body, MAX_BODY_CHARS);

    if !ex.nofollow {
        let mut seen = std::collections::HashSet::new();
        for a in doc.select(&s.anchors) {
            if ex.links.len() >= MAX_LINKS {
                break;
            }
            let rel = a.attr("rel").unwrap_or("").to_ascii_lowercase();
            if rel.contains("nofollow") || rel.contains("sponsored") || rel.contains("ugc") {
                continue;
            }
            let Some(u) = a.attr("href").and_then(|h| urlnorm::resolve(&base, h)) else {
                continue;
            };
            let mut anchor = text_of(a);
            if anchor.is_empty() {
                anchor = a
                    .attr("title")
                    .or_else(|| a.attr("aria-label"))
                    .map(collapse_ws)
                    .unwrap_or_default();
            }
            if anchor.is_empty() {
                if let Some(img) = a
                    .descendants()
                    .filter_map(ElementRef::wrap)
                    .find(|e| e.value().name() == "img")
                {
                    anchor = collapse_ws(img.value().attr("alt").unwrap_or(""));
                }
            }
            truncate_chars(&mut anchor, 200);
            let us = u.to_string();
            if seen.insert(us.clone()) {
                ex.links.push(Link { url: us, anchor });
            }
        }
    }
    ex
}

/// Ermittelt die Zeichenkodierung (HTTP-Header, BOM, `<meta charset>`) und dekodiert.
pub fn decode_body(bytes: &[u8], content_type: Option<&str>) -> String {
    let from_header = content_type
        .and_then(|ct| {
            ct.to_ascii_lowercase().split("charset=").nth(1).map(|c| {
                c.trim_matches(|c: char| c == '"' || c == '\'' || c.is_whitespace())
                    .to_string()
            })
        })
        .and_then(|c| encoding_rs::Encoding::for_label(c.as_bytes()));
    let enc = from_header
        .or_else(|| encoding_rs::Encoding::for_bom(bytes).map(|(e, _)| e))
        .or_else(|| sniff_meta_charset(&bytes[..bytes.len().min(2048)]))
        .unwrap_or(encoding_rs::UTF_8);
    let (s, _, _) = enc.decode(bytes);
    s.into_owned()
}

fn sniff_meta_charset(head: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    let s = String::from_utf8_lossy(head).to_ascii_lowercase();
    let i = s.find("charset=")?;
    let rest = &s[i + 8..];
    let rest = rest.trim_start_matches(['"', '\'']);
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .unwrap_or(rest.len());
    encoding_rs::Encoding::for_label(&rest.as_bytes()[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts() {
        let html = r#"<html lang="de"><head><title> Test  Seite </title>
            <meta name="description" content="Eine Beschreibung">
            <link rel="canonical" href="/kanon"></head>
            <body><nav><a href="/nav">Navigation</a></nav>
            <h1>Überschrift</h1><p>Hallo <b>Welt</b>.</p><p>Zweiter Absatz</p>
            <script>var x = 1;</script>
            <a href="https://other.org/x?utm_source=a#f">Externer Link</a>
            <a href="/bild"><img alt="Bildlink"></a>
            <a href="/spam" rel="nofollow">Spam</a>
            <footer>Impressum</footer></body></html>"#;
        let u = Url::parse("https://example.com/seite").unwrap();
        let ex = extract(html, &u);
        assert_eq!(ex.title, "Test Seite");
        assert_eq!(ex.description, "Eine Beschreibung");
        assert_eq!(ex.canonical.as_deref(), Some("https://example.com/kanon"));
        assert_eq!(ex.lang_hint.as_deref(), Some("de"));
        assert_eq!(ex.headings, "Überschrift");
        assert!(ex.body.contains("Hallo Welt."));
        assert!(ex.body.contains("Zweiter Absatz"));
        assert!(!ex.body.contains("var x"));
        assert!(!ex.body.contains("Navigation"));
        assert!(!ex.body.contains("Impressum"));
        let urls: Vec<_> = ex.links.iter().map(|l| l.url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://example.com/nav",
                "https://other.org/x",
                "https://example.com/bild"
            ]
        );
        assert_eq!(ex.links[2].anchor, "Bildlink");
    }

    #[test]
    fn decodes_latin1() {
        let bytes = b"<meta charset=\"iso-8859-1\"><p>Gr\xfc\xdfe</p>";
        assert!(decode_body(bytes, None).contains("Grüße"));
        assert!(decode_body("ä".as_bytes(), Some("text/html; charset=utf-8")).contains('ä'));
    }
}
