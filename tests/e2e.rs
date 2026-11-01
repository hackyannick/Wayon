//! End-to-End: lokale Testseite crawlen → indexieren → suchen.

use axum::response::Html;
use axum::routing::get;
use axum::Router;
use std::time::Duration;
use wayon::crawler::{CrawlConfig, Crawler};
use wayon::index::builder::{build, BuildConfig};
use wayon::index::Index;
use wayon::search::Searcher;

fn page(title: &str, body: &str, extra_head: &str) -> Html<String> {
    Html(format!(
        "<html><head><title>{title}</title>{extra_head}</head><body>{body}</body></html>"
    ))
}

async fn start_site() -> String {
    let app = Router::new()
        .route("/robots.txt", get(|| async { "User-agent: *\nDisallow: /secret\n" }))
        .route(
            "/",
            get(|| async {
                page(
                    "Testportal Startseite",
                    r#"<h1>Willkommen</h1><p>Das ist die Startseite des Testportals.</p>
                    <nav><a href="/rust">Rust Programmiersprache</a> <a href="/suche">Suchmaschinen erklärt</a>
                    <a href="/secret">Geheim</a> <a href="/dup">Kopie</a> <a href="/noindex">Versteckt</a>
                    <a href="/canon">Kanonisch</a> <a href="/bild.png">Bild</a></nav>"#,
                    "",
                )
            }),
        )
        .route(
            "/rust",
            get(|| async {
                page(
                    "Rust – a language empowering everyone",
                    "<p>Rust is a systems programming language focused on memory safety and speed. \
                     Rust achieves memory safety without a garbage collector. The borrow checker validates references.</p>\
                     <a href='/suche'>search engines</a>",
                    "<meta name='description' content='Rust programming language overview'>",
                )
            }),
        )
        .route(
            "/dup",
            get(|| async {
                page(
                    "Rust copy",
                    "<p>Rust is a systems programming language focused on memory safety and speed. \
                     Rust achieves memory safety without a garbage collector. The borrow checker validates references.</p>\
                     <a href='/suche'>search engines</a>",
                    "<meta name='description' content='Rust programming language overview'>",
                )
            }),
        )
        .route(
            "/suche",
            get(|| async {
                page(
                    "Wie funktioniert eine Suchmaschine?",
                    "<h2>Crawler und Index</h2><p>Eine Suchmaschine besteht aus einem Crawler, der Webseiten lädt, \
                     und einem invertierten Index. Suchmaschinen bewerten Seiten nach Relevanz und Linkpopularität. \
                     Der Crawler folgt Links von Seite zu Seite.</p><a href='/'>Zurück zum Testportal</a>",
                    "<meta name='description' content='Grundlagen von Suchmaschinen'>",
                )
            }),
        )
        .route("/secret", get(|| async { page("Geheim", "<p>geheiminformation</p>", "") }))
        .route(
            "/noindex",
            get(|| async { page("Versteckt", "<p>unsichtbarwort</p>", "<meta name='robots' content='noindex'>") }),
        )
        .route(
            "/canon",
            get(|| async {
                page("Kanonisch", "<p>kanonwort Suchmaschine</p>", "<link rel='canonical' href='/suche'>")
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/")
}

#[test]
fn crawl_index_search() {
    let dir = tempfile::tempdir().unwrap();
    let crawl_dir = dir.path().join("crawl");
    let index_dir = dir.path().join("index");
    let rt = tokio::runtime::Runtime::new().unwrap();
    let stats = rt.block_on(async {
        let seed = start_site().await;
        let cfg = CrawlConfig {
            seeds: vec![seed],
            store_dir: crawl_dir.clone(),
            max_pages: 100,
            host_delay: Duration::from_millis(0),
            use_env_proxy: false,
            concurrency: 4,
            ..Default::default()
        };
        Crawler::new(cfg).unwrap().run().await.unwrap()
    });
    // /, /rust, /dup, /suche, /noindex, /canon (nicht: /secret wegen robots.txt, /bild.png)
    assert_eq!(stats.stored, 6, "{stats:?}");

    let meta = build(&BuildConfig {
        store_dir: crawl_dir.clone(),
        index_dir: index_dir.clone(),
        mem_budget_bytes: 1 << 20,
    })
    .unwrap();
    // noindex raus, /dup als Duplikat von /rust, /canon zeigt auf /suche
    assert_eq!(meta.num_docs, 3);

    let index = Index::open(&index_dir).unwrap();
    let s = Searcher::new(&index);

    let r = s.search("suchmaschine", 0, 10);
    assert!(r.total >= 1);
    assert!(r.hits[0].url.ends_with("/suche"), "{:?}", r.hits);
    assert!(r.hits[0].snippet.contains("<b>"), "{}", r.hits[0].snippet);

    // Stemming: "Suchmaschinen" findet "Suchmaschine"
    assert!(s.search("Suchmaschinen Crawler", 0, 10).hits[0]
        .url
        .ends_with("/suche"));

    // Englisch + Phrase
    let r = s.search("\"memory safety\"", 0, 10);
    assert_eq!(r.total, 1);
    // /dup und /rust sind inhaltsgleich; die kürzere URL bleibt als Repräsentant
    assert!(r.hits[0].url.ends_with("/dup"), "{:?}", r.hits);
    assert_eq!(s.search("\"safety memory\"", 0, 10).total, 0);

    // Ausschluss
    assert!(s
        .search("rust", 0, 10)
        .hits
        .iter()
        .any(|h| h.url.ends_with("/dup")));
    assert!(!s
        .search("rust -garbage", 0, 10)
        .hits
        .iter()
        .any(|h| h.url.ends_with("/dup")));

    // site:-Filter
    assert_eq!(s.search("rust site:example.org", 0, 10).total, 0);
    assert!(s.search("rust site:127.0.0.1", 0, 10).total >= 1);

    // Nicht indexiert
    assert_eq!(s.search("geheiminformation", 0, 10).total, 0);
    assert_eq!(s.search("unsichtbarwort", 0, 10).total, 0);

    // Tippfehler → automatische Korrektur
    let r = s.search("suchmachine", 0, 10);
    assert_eq!(r.corrected.as_deref(), Some("suchmaschine"), "{r:?}");
    assert!(!r.hits.is_empty());

    // Ankertext: "Testportal" steht auf / im Titel und als Linktext auf /suche
    let r = s.search("testportal", 0, 10);
    assert!(r.hits[0].url.ends_with('/'), "{:?}", r.hits);

    // Autovervollständigung
    let c = s.complete("such", 5);
    assert!(c.iter().any(|w| w.starts_with("suchmaschine")), "{c:?}");
}
