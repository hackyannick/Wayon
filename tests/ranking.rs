//! Ranking-Qualität: kleine, realitätsnahe Korpora mit klaren Erwartungen.
//!
//! Nachgestellt nach einer echten Suche nach „windows 2000“, bei der Seiten
//! mit zufälligen Erwähnungen (Fußzeilen-Hilfeseite, Release-Plan mit vielen
//! Jahreszahlen, „foobar 2000“) vor dem eigentlichen Artikel landeten.

use wayon::index::builder::{build, BuildConfig};
use wayon::index::Index;
use wayon::search::Searcher;
use wayon::store::{Link, PageRecord, StoreWriter};

fn page(url: &str, title: &str, body: &str, links: &[(&str, &str)]) -> PageRecord {
    PageRecord {
        url: url.to_string(),
        fetched_at: 1,
        status: 200,
        lang: 2, // Englisch
        noindex: false,
        canonical: None,
        title: title.to_string(),
        description: String::new(),
        headings: String::new(),
        body: body.to_string(),
        links: links
            .iter()
            .map(|(u, a)| Link {
                url: u.to_string(),
                anchor: a.to_string(),
            })
            .collect(),
    }
}

fn corpus(with_article: bool) -> Vec<PageRecord> {
    let mut v = Vec::new();
    let filler =
        "The project documentation describes configuration, installation and usage in detail. ";

    // Der eigentlich gesuchte Artikel (fehlt in kleinen Crawls oft)
    if with_article {
        v.push(page(
        "https://wiki.example/wiki/Windows_2000",
        "Windows 2000 – Wikipedia",
        &format!(
            "Windows 2000 is a major release of the Windows NT operating system developed by Microsoft. \
             Windows 2000 was released to manufacturing in December 1999 and launched to retail in February 2000. \
             {} Four editions of Windows 2000 were released: Professional, Server, Advanced Server and Datacenter Server. \
             Windows 2000 introduced Active Directory. {}",
            filler.repeat(5),
            filler.repeat(5)
        ),
        &[("https://wiki.example/wiki/Microsoft_Windows", "Microsoft Windows")],
    ));
    }
    v.push(page(
        "https://wiki.example/wiki/Microsoft_Windows",
        "Microsoft Windows – Wikipedia",
        &format!(
            "Microsoft Windows is a family of operating systems. Versions include Windows XP, Windows Vista and Windows 10. {}",
            filler.repeat(6)
        ),
        &[("https://wiki.example/wiki/Windows_2000", "Windows 2000")],
    ));
    for i in 0..8 {
        v.push(page(
            &format!("https://wiki.example/wiki/Thema_{i}"),
            &format!("Thema {i} – Wikipedia"),
            &format!("Ein Artikel über Thema {i}. {}", filler.repeat(4)),
            &[
                ("https://wiki.example/wiki/Windows_2000", "Windows 2000"),
                ("https://wiki.example/wiki/Microsoft_Windows", "Windows"),
            ],
        ));
    }

    // Relevante Fachartikel
    v.push(page(
        "https://www.exploit.example/exploits/9541",
        "Microsoft IIS 5.0 FTP Server (Windows 2000) - Remote Stack Overflow",
        "Microsoft IIS 5.0 FTP Server (Windows 2000) - Remote Stack Overflow. Remote exploit for Windows platform.",
        &[],
    ));
    v.push(page(
        "https://www.xda.example/windows-has-been-counting-every-app-since-2000/",
        "Windows has been counting every app you open since 2000",
        &format!(
            "It has been doing so since Windows 2000. It all started with a Windows 2000 Start menu feature. {}",
            filler.repeat(3)
        ),
        &[],
    ));

    v.push(page(
        "https://danluu.example/microsoft-culture/",
        "Windows: a software engineering odyssey",
        &format!(
            "Culture was commonly understood by all. {} Windows 2000 had 1400 developers. \
             Mass assimilation of other teams into the NT team. Windows 2000 shipped late. {}",
            filler.repeat(6),
            filler.repeat(6)
        ),
        &[],
    ));

    // Störer 1: Hilfeseite, die jede Seite der Website in der Fußzeile verlinkt
    v.push(page(
        "https://plato.example/special-characters.html",
        "Displaying Special Characters",
        "To display the widest range of Unicode characters use a recent browser. \
         Microsoft Windows XP, NT, 2000, ME, and 98 Firefox gives the best results. \
         If you are using IE 7 under Windows XP, install the fonts.",
        &[],
    ));
    for i in 0..60 {
        v.push(page(
            &format!("https://plato.example/entries/topic-{i}/"),
            &format!("Philosophy Topic {i} (Encyclopedia)"),
            &format!("An entry on topic {i}. {}", filler.repeat(4)),
            &[
                (
                    "https://plato.example/special-characters.html",
                    "Special Characters",
                ),
                ("https://plato.example/entries/topic-0/", "Topic 0"),
            ],
        ));
    }

    // Störer 2: Release-Plan voller Jahreszahlen
    let dates: String = (1..=28)
        .map(|d| format!("[{d:02}-Sep-2000] task done; "))
        .collect();
    v.push(page(
        "https://peps.example/pep-0200/",
        "PEP 200 – Python 2.0 Release Schedule",
        &format!("Release schedule. {dates} Deal with buffering problem; w9xpopen windows installer issue."),
        &[],
    ));

    // Störer 3: Programmname mit 2000
    v.push(page(
        "https://www.chip.example/download/foobar-2000",
        "foobar 2000 - Kostenloser Download",
        "foobar 2000 ist ein Audio-Player für Windows. foobar 2000 v2.26 Download, foobar 2000 Version.",
        &[],
    ));

    // Füllseiten: „2000“ als Jahreszahl und „windows“ einzeln, damit beide Begriffe häufig sind
    for i in 0..150 {
        v.push(page(
            &format!("https://news.example/archive/{i}"),
            &format!("News archive entry {i}"),
            &format!("Published in the year 2000. {}", filler.repeat(3)),
            &[],
        ));
    }
    for i in 0..80 {
        v.push(page(
            &format!("https://howto.example/guide/{i}"),
            &format!("How to configure item {i}"),
            &format!(
                "This guide works on Windows, Linux and macOS. {}",
                filler.repeat(3)
            ),
            &[],
        ));
    }
    v
}

fn build_index(docs: Vec<PageRecord>) -> (tempfile::TempDir, Index) {
    let dir = tempfile::tempdir().unwrap();
    let crawl = dir.path().join("crawl");
    {
        let mut w = StoreWriter::open(&crawl).unwrap();
        for d in &docs {
            w.append(d).unwrap();
        }
        w.flush().unwrap();
    }
    let index_dir = dir.path().join("index");
    build(&BuildConfig {
        store_dir: crawl,
        index_dir: index_dir.clone(),
        mem_budget_bytes: 8 << 20,
    })
    .unwrap();
    let index = Index::open(&index_dir).unwrap();
    (dir, index)
}

fn rank_of(urls: &[String], needle: &str) -> usize {
    urls.iter()
        .position(|u| u.contains(needle))
        .map(|p| p + 1)
        .unwrap_or(usize::MAX)
}

fn show(index: &Index, q: &str) -> Vec<String> {
    let res = Searcher::new(index).search(q, 0, 10);
    eprintln!("--- {q}");
    for (i, h) in res.hits.iter().enumerate() {
        let m = index.doc_meta(h.doc);
        eprintln!(
            "{:>2}. {:7.3}  q={:3}  {}",
            i + 1,
            h.score,
            m.quality,
            h.url
        );
    }
    res.hits.iter().map(|h| h.url.clone()).collect()
}

const NOISE: [&str; 3] = ["special-characters", "pep-0200", "foobar-2000"];

#[test]
fn windows_2000_with_article() {
    let (_dir, index) = build_index(corpus(true));
    let urls = show(&index, "windows 2000");
    assert_eq!(
        rank_of(&urls, "wiki/Windows_2000"),
        1,
        "Artikel muss vorne stehen"
    );
    let worst_good = ["exploit.example", "xda.example", "danluu.example"]
        .iter()
        .map(|g| rank_of(&urls, g))
        .max()
        .unwrap();
    for bad in NOISE {
        assert!(
            rank_of(&urls, bad) > worst_good,
            "{bad} vor echtem Treffer: {urls:#?}"
        );
    }
}

#[test]
fn windows_2000_without_article() {
    // Wie im echten Index: kein eigener Artikel – dann müssen Seiten, die
    // „Windows 2000“ wörtlich behandeln, vor zufälligen Erwähnungen stehen.
    let (_dir, index) = build_index(corpus(false));
    let urls = show(&index, "windows 2000");
    let worst_good = ["exploit.example", "xda.example", "danluu.example"]
        .iter()
        .map(|g| rank_of(&urls, g))
        .max()
        .unwrap();
    assert!(worst_good <= 3, "echte Treffer nicht vorne: {urls:#?}");
    for bad in NOISE {
        assert!(
            rank_of(&urls, bad) > worst_good,
            "{bad} vor echtem Treffer: {urls:#?}"
        );
    }
}

#[test]
fn navigational_query_prefers_homepage() {
    let filler = "Tutorials, examples and reference material for developers. ";
    let mut docs = vec![page(
        "https://www.python.example/",
        "Welcome to Python.example",
        &format!(
            "The official home of the Python programming language. {}",
            filler.repeat(3)
        ),
        &[("https://www.python.example/doc/", "Documentation")],
    )];
    docs.push(page(
        "https://www.python.example/doc/",
        "Python Documentation",
        &format!("Python documentation index. {}", filler.repeat(3)),
        &[("https://www.python.example/", "Python")],
    ));
    for i in 0..40 {
        docs.push(page(
            &format!("https://blog{}.example/python-tips-{i}", i % 7),
            &format!("Python tips and tricks part {i}"),
            &format!(
                "Python python python: learn Python list comprehensions and Python decorators. {}",
                filler.repeat(2)
            ),
            &[("https://www.python.example/", "Python")],
        ));
    }
    let (_dir, index) = build_index(docs);
    let urls = show(&index, "python");
    assert_eq!(rank_of(&urls, "www.python.example/"), 1, "{urls:#?}");
}

#[test]
fn german_inflection_and_title() {
    let filler = "Weitere Informationen finden Sie in der Dokumentation und im Handbuch. ";
    let mut docs = Vec::new();
    let mut de = |url: &str, title: &str, body: &str| {
        let mut p = page(url, title, body, &[]);
        p.lang = 1;
        docs.push(p);
    };
    de(
        "https://wiki.example/Suchmaschine",
        "Suchmaschine – Wikipedia",
        &format!("Eine Suchmaschine ist ein Programm zur Recherche von Dokumenten. Suchmaschinen indexieren Webseiten mit einem Crawler. {}", filler.repeat(4)),
    );
    de(
        "https://shop.example/maschinen",
        "Maschinen günstig kaufen",
        &format!(
            "Wir verkaufen Maschinen aller Art. Suchen Sie nach Ersatzteilen? {}",
            filler.repeat(4)
        ),
    );
    for i in 0..30 {
        de(
            &format!("https://news.example/{i}"),
            &format!("Nachrichten {i}"),
            &format!(
                "Die Suchmaschine wurde in einem Nebensatz erwähnt. {}",
                filler.repeat(6)
            ),
        );
    }
    let (_dir, index) = build_index(docs);
    let urls = show(&index, "Suchmaschinen");
    assert_eq!(rank_of(&urls, "wiki.example/Suchmaschine"), 1, "{urls:#?}");
}
