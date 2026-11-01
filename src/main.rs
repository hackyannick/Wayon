use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;
use wayon::crawler::{CrawlConfig, Crawler};
use wayon::index::builder::{build, BuildConfig};
use wayon::index::{Index, FIELD_NAMES};
use wayon::search::Searcher;

#[derive(Parser)]
#[command(
    name = "wayon",
    version,
    about = "Wayon – ressourcensparende Websuchmaschine mit eigenem Crawler und Index"
)]
struct Cli {
    /// Datenverzeichnis (enthält crawl/ und index/)
    #[arg(long, global = true, default_value = "data")]
    data: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Webseiten crawlen (fortsetzbar; Strg+C beendet sauber)
    Crawl {
        /// Start-URLs
        seeds: Vec<String>,
        /// Datei mit Start-URLs (eine pro Zeile)
        #[arg(long)]
        seeds_file: Option<PathBuf>,
        /// Maximale Anzahl zu ladender Seiten in diesem Lauf
        #[arg(long, default_value_t = 10_000)]
        max_pages: u64,
        /// Maximale Linktiefe ab den Seeds
        #[arg(long, default_value_t = 8)]
        max_depth: u32,
        /// Maximale Seiten pro Host
        #[arg(long, default_value_t = 5_000)]
        max_per_host: u32,
        /// Gleichzeitige Verbindungen insgesamt
        #[arg(long, default_value_t = 64)]
        concurrency: usize,
        /// Pause zwischen Anfragen an denselben Host (ms)
        #[arg(long, default_value_t = 1000)]
        delay_ms: u64,
        /// Nur die Domains der Seeds (inkl. Subdomains) crawlen
        #[arg(long)]
        stay_on_domains: bool,
        /// Nur diese Domains crawlen (kommagetrennt)
        #[arg(long, value_delimiter = ',')]
        domains: Vec<String>,
        /// robots.txt ignorieren (nur für eigene Seiten verwenden!)
        #[arg(long)]
        ignore_robots: bool,
        /// Keine Sitemaps lesen
        #[arg(long)]
        no_sitemaps: bool,
        /// Keinen System-Proxy verwenden
        #[arg(long)]
        no_proxy: bool,
    },
    /// Index aus den gecrawlten Seiten (neu) aufbauen
    Index {
        /// RAM-Budget für den Aufbau in MiB
        #[arg(long, default_value_t = 512)]
        mem_mb: usize,
    },
    /// Web-Oberfläche und API starten
    Serve {
        #[arg(long, default_value = "127.0.0.1:8080")]
        addr: std::net::SocketAddr,
    },
    /// Im Terminal suchen
    Search {
        query: Vec<String>,
        #[arg(short, long, default_value_t = 10)]
        n: usize,
        #[arg(long, default_value_t = 1)]
        page: usize,
    },
    /// Statistiken zum Index anzeigen
    Stats,
    /// Gecrawlte Seiten anzeigen (zur Kontrolle)
    Dump {
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Nur URLs, die diesen Text enthalten
        #[arg(long)]
        filter: Option<String>,
    },
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_target(false)
        .init();
    let cli = Cli::parse();
    let crawl_dir = cli.data.join("crawl");
    let index_dir = cli.data.join("index");
    match cli.cmd {
        Cmd::Crawl {
            mut seeds,
            seeds_file,
            max_pages,
            max_depth,
            max_per_host,
            concurrency,
            delay_ms,
            stay_on_domains,
            mut domains,
            ignore_robots,
            no_sitemaps,
            no_proxy,
        } => {
            if let Some(f) = seeds_file {
                let txt = std::fs::read_to_string(&f)
                    .with_context(|| format!("{} nicht lesbar", f.display()))?;
                seeds.extend(
                    txt.lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty() && !l.starts_with('#'))
                        .map(String::from),
                );
            }
            if stay_on_domains {
                for s in &seeds {
                    let s = if s.contains("://") {
                        s.clone()
                    } else {
                        format!("https://{s}")
                    };
                    if let Ok(u) = url::Url::parse(&s) {
                        domains.push(wayon::urlnorm::host_key(&u));
                    }
                }
            }
            let cfg = CrawlConfig {
                seeds,
                store_dir: crawl_dir,
                max_pages,
                max_depth,
                max_pages_per_host: max_per_host,
                concurrency: concurrency.max(1),
                host_delay: Duration::from_millis(delay_ms),
                allowed_domains: domains
                    .into_iter()
                    .map(|d| d.trim().trim_start_matches("www.").to_ascii_lowercase())
                    .collect(),
                respect_robots: !ignore_robots,
                read_sitemaps: !no_sitemaps,
                use_env_proxy: !no_proxy,
                ..Default::default()
            };
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(num_threads(4))
                .enable_all()
                .build()?;
            rt.block_on(async { Crawler::new(cfg)?.run().await })?;
        }
        Cmd::Index { mem_mb } => {
            let meta = build(&BuildConfig {
                store_dir: crawl_dir,
                index_dir: index_dir.clone(),
                mem_budget_bytes: mem_mb << 20,
            })?;
            println!(
                "Index fertig: {} Dokumente, {} Terme",
                meta.num_docs, meta.num_terms
            );
            print_sizes(&Index::open(&index_dir)?);
        }
        Cmd::Serve { addr } => {
            let index = Index::open(&index_dir)?;
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(num_threads(4))
                .enable_all()
                .build()?;
            rt.block_on(wayon::server::serve(index, addr))?;
        }
        Cmd::Search { query, n, page } => {
            let index = Index::open(&index_dir)?;
            let q = query.join(" ");
            let res = Searcher::new(&index).search(&q, (page.max(1) - 1) * n, n);
            if let Some(c) = &res.corrected {
                println!("Ergebnisse für „{c}“:");
            } else if let Some(s) = &res.suggestion {
                println!("Meinten Sie: {s}");
            }
            println!(
                "Ungefähr {} Ergebnisse ({:.2} ms)\n",
                res.total, res.took_ms
            );
            for (i, h) in res.hits.iter().enumerate() {
                println!(
                    "{:>3}. {}\n     {}\n     {}\n",
                    res.offset + i + 1,
                    h.title,
                    h.url,
                    strip_tags(&h.snippet)
                );
            }
        }
        Cmd::Dump { limit, filter } => {
            let mut shown = 0;
            let mut total = 0u64;
            wayon::store::for_each_record(&crawl_dir, |_, r| {
                total += 1;
                if shown < limit && filter.as_ref().is_none_or(|f| r.url.contains(f.as_str())) {
                    shown += 1;
                    println!(
                        "{} [{}] lang={} noindex={} links={}\n  Titel: {}\n  Beschr.: {}\n  Überschr.: {}\n  Text ({} Zeichen): {}\n",
                        r.url, r.status, wayon::text::Lang::from_u8(r.lang).code(), r.noindex, r.links.len(), r.title,
                        wayon::crawler::prefix(&r.description, 150), wayon::crawler::prefix(&r.headings, 150),
                        r.body.len(), wayon::crawler::prefix(&r.body, 300)
                    );
                }
                Ok(())
            })?;
            println!("{total} Records insgesamt");
        }
        Cmd::Stats => {
            let index = Index::open(&index_dir)?;
            let m = &index.meta;
            println!(
                "Dokumente: {}\nTerme:     {}\nHosts:     {}",
                m.num_docs,
                m.num_terms,
                index.num_hosts()
            );
            for (i, name) in FIELD_NAMES.iter().enumerate() {
                println!("Ø Länge {name:<9} {:.1}", m.avg_len[i]);
            }
            print_sizes(&index);
        }
    }
    Ok(())
}

fn num_threads(max: usize) -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .min(max)
}

fn print_sizes(index: &Index) {
    let sizes = index.file_sizes();
    let total: u64 = sizes.iter().map(|s| s.1).sum();
    println!("\nIndexgröße:");
    for (n, s) in &sizes {
        println!("  {n:<14} {:>10.2} MiB", *s as f64 / 1048576.0);
    }
    let n = index.num_docs().max(1) as f64;
    let inv: u64 = sizes
        .iter()
        .filter(|s| matches!(s.0, "terms.fst" | "postings.bin" | "docmeta.bin"))
        .map(|s| s.1)
        .sum();
    println!("  {:<14} {:>10.2} MiB  ({:.0} Byte/Dokument, davon invertierter Index {:.0} Byte/Dokument)", "gesamt", total as f64 / 1048576.0, total as f64 / n, inv as f64 / n);
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}
