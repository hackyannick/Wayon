//! Importer-Test mit Dateien im Originalformat von Common Crawl (WARC, jeder
//! Record ein eigenes gzip-Member) und Wikimedia-Enterprise-HTML-Dumps
//! (tar.gz mit NDJSON).

use flate2::write::GzEncoder;
use flate2::Compression;
use std::io::Write;
use wayon::import::{commoncrawl, wikipedia, LangFilter};
use wayon::index::builder::{build, BuildConfig};
use wayon::index::Index;
use wayon::search::Searcher;

fn gz_member(data: &[u8]) -> Vec<u8> {
    let mut e = GzEncoder::new(Vec::new(), Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn warc_record(wtype: &str, uri: &str, payload: &[u8]) -> Vec<u8> {
    let mut r = format!(
        "WARC/1.0\r\nWARC-Type: {wtype}\r\nWARC-Target-URI: {uri}\r\nWARC-Date: 2026-10-05T12:00:00Z\r\nWARC-Record-ID: <urn:uuid:x>\r\nContent-Length: {}\r\n\r\n",
        payload.len()
    )
    .into_bytes();
    r.extend_from_slice(payload);
    r.extend_from_slice(b"\r\n\r\n");
    gz_member(&r)
}

fn http(status: &str, ct: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nX-Crawler-Content-Encoding: gzip\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn html(title: &str, body: &str) -> String {
    format!("<html><head><title>{title}</title></head><body><p>{body}</p></body></html>")
}

#[test]
fn imports_warc_and_wikipedia() {
    let dir = tempfile::tempdir().unwrap();
    let crawl = dir.path().join("crawl");

    // ---------- Common Crawl (WARC) ----------
    let en = "Rust is a systems programming language focused on memory safety. The borrow checker validates references at compile time. ".repeat(3);
    let de = "Eine Suchmaschine durchsucht das Web mit einem Crawler und speichert die Seiten in einem invertierten Index. ".repeat(3);
    let fr = "Le moteur de recherche parcourt le web avec un robot et enregistre les pages dans un index inversé. ".repeat(3);
    let mut warc = warc_record("warcinfo", "", b"software: test\r\n");
    for (uri, payload) in [
        (
            "https://rust.example/",
            http(
                "200 OK",
                "text/html; charset=utf-8",
                &html("Rust language", &en),
            ),
        ),
        (
            "https://suche.example/crawler",
            http("200 OK", "text/html", &html("Suchmaschinen erklärt", &de)),
        ),
        (
            "https://moteur.example/",
            http("200 OK", "text/html", &html("Moteur de recherche", &fr)),
        ),
        (
            "https://rust.example/logo.png",
            http("200 OK", "image/png", "PNGDATA"),
        ),
        (
            "https://rust.example/missing",
            http("404 Not Found", "text/html", &html("Not found", &en)),
        ),
    ] {
        warc.extend(warc_record(
            "request",
            uri,
            b"GET / HTTP/1.1\r\nHost: x\r\n\r\n",
        ));
        warc.extend(warc_record("response", uri, &payload));
        warc.extend(warc_record("metadata", uri, b"fetchTimeMs: 12\r\n"));
    }
    let warc_path = dir.path().join("test.warc.gz");
    std::fs::write(&warc_path, &warc).unwrap();
    let stats = commoncrawl::import_warc_files(
        &[warc_path.to_string_lossy().to_string()],
        &crawl,
        LangFilter::parse("de,en"),
    )
    .unwrap();
    assert_eq!(stats.seen, 5, "{stats:?}"); // 5 HTTP-Antworten
    assert_eq!(stats.stored, 2, "{stats:?}"); // Englisch + Deutsch; Französisch, PNG, 404 gefiltert

    // ---------- Wikipedia (Enterprise-HTML-Dump) ----------
    let article = |name: &str, slug: &str, text: &str, links: &[(&str, &str)], ns: i64| {
        let a: String = links
            .iter()
            .map(|(s, t)| format!(r#" <a rel="mw:WikiLink" href="./{s}" title="{t}">{t}</a>"#))
            .collect();
        let html = format!(
            r#"<!DOCTYPE html><html lang="de"><head><title>{name}</title><base href="//de.wikipedia.org/wiki/"/></head><body><section><p>{text}{a}</p></section><div role="navigation" class="navbox">Navigationsleiste Betriebssysteme</div></body></html>"#
        );
        serde_json::json!({
            "name": name,
            "url": format!("https://de.wikipedia.org/wiki/{slug}"),
            "date_modified": "2026-10-01T08:00:00Z",
            "abstract": format!("{name} – Kurzfassung."),
            "namespace": {"identifier": ns},
            "in_language": {"identifier": "de"},
            "article_body": {"html": html, "wikitext": "…"}
        })
        .to_string()
    };
    let lines = [
        article(
            "Windows 2000",
            "Windows_2000",
            &"Windows 2000 ist ein Betriebssystem von Microsoft und Nachfolger von Windows NT 4.0. ".repeat(3),
            &[("Microsoft_Windows", "Microsoft Windows")],
            0,
        ),
        article(
            "Microsoft Windows",
            "Microsoft_Windows",
            &"Microsoft Windows ist eine Familie von Betriebssystemen. Versionen sind Windows XP und Windows 10. ".repeat(3),
            &[("Windows_2000", "Windows 2000")],
            0,
        ),
        article("Diskussion:Windows 2000", "Diskussion:Windows_2000", &"Diskussionsseite ".repeat(20), &[], 1),
    ];
    let ndjson = lines.join("\n") + "\n";
    let mut tar_gz = Vec::new();
    {
        let enc = GzEncoder::new(&mut tar_gz, Compression::default());
        let mut t = tar::Builder::new(enc);
        let mut h = tar::Header::new_gnu();
        h.set_size(ndjson.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        t.append_data(&mut h, "dewiki_namespace_0_0.ndjson", ndjson.as_bytes())
            .unwrap();
        t.into_inner().unwrap().finish().unwrap();
    }
    let dump = dir
        .path()
        .join("dewiki-NS0-20261101-ENTERPRISE-HTML.json.tar.gz");
    std::fs::write(&dump, &tar_gz).unwrap();
    let ws = wikipedia::import_wikipedia(&dump.to_string_lossy(), &crawl).unwrap();
    assert_eq!(ws.seen, 3, "{ws:?}");
    assert_eq!(ws.stored, 2, "{ws:?}"); // Diskussionsseite (Namensraum 1) gefiltert

    // ---------- Index und Suche ----------
    let index_dir = dir.path().join("index");
    let meta = build(&BuildConfig {
        store_dir: crawl,
        index_dir: index_dir.clone(),
        mem_budget_bytes: 8 << 20,
    })
    .unwrap();
    assert_eq!(meta.num_docs, 4);
    let index = Index::open(&index_dir).unwrap();
    let s = Searcher::new(&index);

    let r = s.search("windows 2000", 0, 10);
    assert_eq!(
        r.hits[0].url, "https://de.wikipedia.org/wiki/Windows_2000",
        "{:?}",
        r.hits
    );
    assert_eq!(r.hits[0].title, "Windows 2000");
    // Navigationsleiste wurde nicht als Text übernommen
    assert_eq!(s.search("Navigationsleiste", 0, 10).total, 0);
    assert_eq!(
        s.search("borrow checker", 0, 10).hits[0].url,
        "https://rust.example/"
    );
    assert_eq!(
        s.search("Suchmaschine Crawler", 0, 10).hits[0].url,
        "https://suche.example/crawler"
    );
    assert_eq!(s.search("moteur recherche", 0, 10).total, 0);
}
