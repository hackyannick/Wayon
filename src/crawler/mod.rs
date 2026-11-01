//! Asynchroner, höflicher Web-Crawler.
//!
//! * Pro Host eine Warteschlange (Breitensuche), höchstens eine Anfrage gleichzeitig
//!   pro Host und eine Mindestpause zwischen Anfragen (oder `Crawl-delay`).
//! * robots.txt wird pro Origin geladen und beachtet, Sitemaps werden gelesen.
//! * Bereits gesehene URLs merkt sich ein Bloom-Filter (sehr speichersparend).
//! * Ergebnisse landen im append-only [`crate::store`]. Mit Strg+C wird sauber
//!   beendet und die Warteschlange gespeichert, sodass der nächste Lauf fortsetzt.

pub mod bloom;
pub mod html;
pub mod robots;

use crate::store::{self, PageRecord, StoreWriter};
use crate::text;
use crate::urlnorm;
use anyhow::{Context, Result};
use bloom::Bloom;
use robots::Robots;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use url::Url;

#[derive(Clone, Debug)]
pub struct CrawlConfig {
    pub seeds: Vec<String>,
    /// Verzeichnis des Crawl-Speichers.
    pub store_dir: PathBuf,
    pub max_pages: u64,
    pub max_depth: u32,
    pub max_pages_per_host: u32,
    pub concurrency: usize,
    /// Mindestabstand zwischen zwei Anfragen an denselben Host.
    pub host_delay: Duration,
    /// Wenn nicht leer: nur diese Domains (inkl. Subdomains) crawlen.
    pub allowed_domains: Vec<String>,
    pub user_agent: String,
    pub timeout: Duration,
    pub max_body_bytes: usize,
    pub use_env_proxy: bool,
    pub respect_robots: bool,
    pub read_sitemaps: bool,
    pub expected_urls: u64,
    pub max_queue_per_host: usize,
    pub max_total_queue: usize,
}

impl Default for CrawlConfig {
    fn default() -> Self {
        CrawlConfig {
            seeds: Vec::new(),
            store_dir: PathBuf::from("data/crawl"),
            max_pages: 10_000,
            max_depth: 8,
            max_pages_per_host: 5_000,
            concurrency: 64,
            host_delay: Duration::from_millis(1000),
            allowed_domains: Vec::new(),
            user_agent: format!(
                "WayonBot/{} (+https://github.com/hackyannick/wayon)",
                env!("CARGO_PKG_VERSION")
            ),
            timeout: Duration::from_secs(20),
            max_body_bytes: 3 * 1024 * 1024,
            use_env_proxy: true,
            respect_robots: true,
            read_sitemaps: true,
            expected_urls: 10_000_000,
            max_queue_per_host: 20_000,
            max_total_queue: 3_000_000,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct CrawlStats {
    pub fetched: u64,
    pub stored: u64,
    pub errors: u64,
    pub skipped: u64,
    pub bytes: u64,
}

enum RobotsState {
    Unknown,
    Ready(Robots),
}

struct HostState {
    origin: Url,
    queue: VecDeque<(String, u32)>,
    robots: RobotsState,
    crawled: u32,
    /// Aktuelle Pause (wächst bei 429/503, schrumpft danach wieder bis `base_delay`).
    delay: Duration,
    base_delay: Duration,
    busy: bool,
    scheduled: bool,
    /// Frühester Zeitpunkt der nächsten Anfrage (Politeness).
    next_ok: Instant,
}

enum Msg {
    Robots {
        origin: String,
        robots: Robots,
    },
    Sitemap {
        origin: String,
        urls: Vec<String>,
    },
    Page {
        origin: String,
        depth: u32,
        result: Result<FetchResult>,
    },
}

enum FetchResult {
    Html {
        record: PageRecord,
        bytes: u64,
        redirected_from: Option<String>,
    },
    Skipped {
        status: u16,
        retry_after: Option<u64>,
    },
}

struct Frontier {
    hosts: HashMap<String, HostState>,
    ready: BinaryHeap<Reverse<(Instant, u64, String)>>,
    seq: u64,
    seen: Bloom,
    queued: usize,
}

impl Frontier {
    fn schedule(&mut self, origin: &str, at: Instant) {
        if let Some(h) = self.hosts.get_mut(origin) {
            if !h.scheduled && !h.busy {
                h.scheduled = true;
                self.seq += 1;
                self.ready
                    .push(Reverse((at.max(h.next_ok), self.seq, origin.to_string())));
            }
        }
    }
}

pub struct Crawler {
    cfg: CrawlConfig,
    client: reqwest::Client,
    agent_token: String,
}

fn origin_of(u: &Url) -> String {
    u.origin().ascii_serialization()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Crawler {
    pub fn new(cfg: CrawlConfig) -> Result<Crawler> {
        let mut b = reqwest::Client::builder()
            .user_agent(cfg.user_agent.clone())
            .timeout(cfg.timeout)
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(1)
            .pool_idle_timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(5));
        if !cfg.use_env_proxy {
            b = b.no_proxy();
        }
        let client = b
            .build()
            .context("HTTP-Client konnte nicht erstellt werden")?;
        let agent_token = cfg
            .user_agent
            .split('/')
            .next()
            .unwrap_or("WayonBot")
            .to_string();
        Ok(Crawler {
            cfg,
            client,
            agent_token,
        })
    }

    fn domain_allowed(&self, u: &Url) -> bool {
        if self.cfg.allowed_domains.is_empty() {
            return true;
        }
        let h = u.host_str().unwrap_or("").to_ascii_lowercase();
        self.cfg
            .allowed_domains
            .iter()
            .any(|d| h == *d || h.ends_with(&format!(".{d}")))
    }

    fn enqueue(&self, fr: &mut Frontier, url: &str, depth: u32) -> bool {
        if depth > self.cfg.max_depth || fr.queued >= self.cfg.max_total_queue {
            return false;
        }
        let Ok(u) = Url::parse(url) else { return false };
        let Some(u) = urlnorm::normalize(&u) else {
            return false;
        };
        if !urlnorm::looks_crawlable(&u) || !self.domain_allowed(&u) {
            return false;
        }
        let origin = origin_of(&u);
        let h = fr.hosts.entry(origin.clone()).or_insert_with(|| HostState {
            origin: Url::parse(&origin).unwrap_or_else(|_| u.clone()),
            queue: VecDeque::new(),
            robots: if self.cfg.respect_robots {
                RobotsState::Unknown
            } else {
                RobotsState::Ready(Robots::allow_all())
            },
            crawled: 0,
            delay: self.cfg.host_delay,
            base_delay: self.cfg.host_delay,
            busy: false,
            scheduled: false,
            next_ok: Instant::now(),
        });
        if h.queue.len() >= self.cfg.max_queue_per_host
            || h.crawled as usize + h.queue.len() >= self.cfg.max_pages_per_host as usize
        {
            return false;
        }
        if !fr.seen.insert(u.as_str().as_bytes()) {
            return false;
        }
        h.queue.push_back((u.to_string(), depth));
        fr.queued += 1;
        fr.schedule(&origin, Instant::now());
        true
    }

    fn frontier_path(&self) -> PathBuf {
        self.cfg.store_dir.join("frontier.txt")
    }

    /// Startet den Crawl und läuft, bis `max_pages` erreicht, die Warteschlange leer
    /// ist oder Strg+C gedrückt wurde.
    pub async fn run(self) -> Result<CrawlStats> {
        let this = Arc::new(self);
        let cfg = &this.cfg;
        let mut fr = Frontier {
            hosts: HashMap::new(),
            ready: BinaryHeap::new(),
            seq: 0,
            seen: Bloom::new(cfg.expected_urls, 0.01),
            queued: 0,
        };

        // Fortsetzen: bereits gespeicherte Seiten als gesehen markieren.
        let store_dir = cfg.store_dir.clone();
        let known: Vec<String> = tokio::task::spawn_blocking(move || {
            let mut v = Vec::new();
            store::for_each_record(&store_dir, |_, r| {
                v.push(r.url);
                Ok(())
            })
            .map(|_| v)
        })
        .await??;
        for u in &known {
            fr.seen.insert(u.as_bytes());
        }
        if !known.is_empty() {
            log::info!("{} bereits gecrawlte Seiten im Speicher", known.len());
        }
        drop(known);

        let mut resumed = 0;
        if let Ok(f) = std::fs::File::open(this.frontier_path()) {
            for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
                if let Some((d, u)) = line.split_once(' ') {
                    if this.enqueue(&mut fr, u, d.parse().unwrap_or(0)) {
                        resumed += 1;
                    }
                }
            }
            log::info!("{resumed} URLs aus gespeicherter Warteschlange übernommen");
        }
        for s in &cfg.seeds {
            let s = if s.contains("://") {
                s.clone()
            } else {
                format!("https://{s}")
            };
            if !this.enqueue(&mut fr, &s, 0) {
                log::debug!("Seed übersprungen (bereits gesehen oder ungültig): {s}");
            }
        }
        log::info!(
            "Crawl startet: {} Hosts, {} URLs in der Warteschlange, Bloom-Filter {} KiB",
            fr.hosts.len(),
            fr.queued,
            fr.seen.memory_bytes() / 1024
        );

        let mut writer = StoreWriter::open(&cfg.store_dir)?;
        let (tx, mut rx) = mpsc::channel::<Msg>(cfg.concurrency * 2);
        let mut stats = CrawlStats::default();
        let mut in_flight = 0usize;
        let mut started = 0u64;
        let mut stopping = false;
        let t0 = Instant::now();
        let mut last_report = Instant::now();
        let ctrl_c = shutdown_signal();
        tokio::pin!(ctrl_c);

        loop {
            // Neue Anfragen starten, solange Kapazität frei ist.
            let now = Instant::now();
            while !stopping && in_flight < cfg.concurrency && started < cfg.max_pages {
                let Some(Reverse((at, _, _))) = fr.ready.peek() else {
                    break;
                };
                if *at > now {
                    break;
                }
                let Reverse((_, _, origin)) = fr.ready.pop().unwrap();
                let Some(h) = fr.hosts.get_mut(&origin) else {
                    continue;
                };
                h.scheduled = false;
                if matches!(h.robots, RobotsState::Unknown) {
                    h.busy = true;
                    in_flight += 1;
                    let (c, tx, o, read_sm) = (
                        this.clone(),
                        tx.clone(),
                        h.origin.clone(),
                        cfg.read_sitemaps,
                    );
                    tokio::spawn(async move {
                        let robots = c.fetch_robots(&o).await;
                        let sitemaps = if read_sm {
                            robots.sitemaps.clone()
                        } else {
                            Vec::new()
                        };
                        let _ = tx
                            .send(Msg::Robots {
                                origin: origin_of(&o),
                                robots,
                            })
                            .await;
                        if !sitemaps.is_empty() {
                            let urls = c.fetch_sitemaps(&sitemaps).await;
                            let _ = tx
                                .send(Msg::Sitemap {
                                    origin: origin_of(&o),
                                    urls,
                                })
                                .await;
                        }
                    });
                    continue;
                }
                let Some((url, depth)) = h.queue.pop_front() else {
                    continue;
                };
                fr.queued -= 1;
                let allowed = match &h.robots {
                    RobotsState::Ready(r) => {
                        let u = Url::parse(&url).ok();
                        u.map(|u| r.is_allowed(&u[url::Position::BeforePath..]))
                            .unwrap_or(false)
                    }
                    RobotsState::Unknown => true,
                };
                if !allowed {
                    stats.skipped += 1;
                    if !h.queue.is_empty() {
                        fr.schedule(&origin, now);
                    }
                    continue;
                }
                h.busy = true;
                h.crawled += 1;
                in_flight += 1;
                started += 1;
                let (c, tx) = (this.clone(), tx.clone());
                tokio::spawn(async move {
                    let result = c.fetch_page(&url).await;
                    let _ = tx
                        .send(Msg::Page {
                            origin,
                            depth,
                            result,
                        })
                        .await;
                });
            }

            if in_flight == 0 && (stopping || fr.ready.is_empty() || started >= cfg.max_pages) {
                break;
            }

            let next_wake = fr.ready.peek().map(|Reverse((at, _, _))| *at);
            let can_start = !stopping && in_flight < cfg.concurrency && started < cfg.max_pages;
            tokio::select! {
                msg = rx.recv() => {
                    let Some(msg) = msg else { break };
                    this.handle(msg, &mut fr, &mut writer, &mut stats, &mut in_flight)?;
                }
                _ = async {
                    match next_wake {
                        Some(t) if can_start => tokio::time::sleep_until(t.into()).await,
                        _ => std::future::pending::<()>().await,
                    }
                } => {}
                _ = &mut ctrl_c, if !stopping => {
                    log::warn!("Abbruchsignal – beende laufende Anfragen und speichere Warteschlange …");
                    stopping = true;
                }
            }

            if last_report.elapsed() >= Duration::from_secs(10) {
                last_report = Instant::now();
                let secs = t0.elapsed().as_secs_f64().max(0.001);
                log::info!(
                    "{} geladen, {} gespeichert, {} Fehler, {} übersprungen | {:.1} Seiten/s | {} URLs in {} Hosts wartend",
                    stats.fetched, stats.stored, stats.errors, stats.skipped,
                    stats.fetched as f64 / secs, fr.queued, fr.hosts.len()
                );
                writer.flush()?;
            }
        }
        writer.flush()?;
        this.save_frontier(&fr)?;
        log::info!(
            "Crawl beendet: {} Seiten geladen, {} gespeichert, {} Fehler, {:.1} MiB in {:.0?}",
            stats.fetched,
            stats.stored,
            stats.errors,
            stats.bytes as f64 / 1048576.0,
            t0.elapsed()
        );
        Ok(stats)
    }

    fn handle(
        &self,
        msg: Msg,
        fr: &mut Frontier,
        writer: &mut StoreWriter,
        stats: &mut CrawlStats,
        in_flight: &mut usize,
    ) -> Result<()> {
        let now = Instant::now();
        match msg {
            Msg::Robots { origin, robots } => {
                *in_flight -= 1;
                if let Some(h) = fr.hosts.get_mut(&origin) {
                    if let Some(d) = robots.crawl_delay {
                        h.base_delay = h
                            .base_delay
                            .max(Duration::from_secs_f64(d.clamp(0.0, 30.0)));
                        h.delay = h.delay.max(h.base_delay);
                    }
                    h.robots = RobotsState::Ready(robots);
                    h.busy = false;
                }
                fr.schedule(&origin, now);
            }
            Msg::Sitemap { origin, urls } => {
                let mut n = 0;
                for u in urls {
                    let same_origin = Url::parse(&u)
                        .map(|x| origin_of(&x) == origin)
                        .unwrap_or(false);
                    if same_origin && self.enqueue(fr, &u, 1) {
                        n += 1;
                    }
                }
                log::debug!("{origin}: {n} URLs aus Sitemaps");
            }
            Msg::Page {
                origin,
                depth,
                result,
            } => {
                *in_flight -= 1;
                let mut next = now;
                if let Some(h) = fr.hosts.get_mut(&origin) {
                    h.busy = false;
                    next = now + h.delay;
                    h.next_ok = next;
                }
                match result {
                    Ok(FetchResult::Html {
                        record,
                        bytes,
                        redirected_from,
                    }) => {
                        stats.fetched += 1;
                        if let Some(h) = fr.hosts.get_mut(&origin) {
                            // Nach erfolgreichen Anfragen die Pause langsam wieder verkürzen
                            h.delay = h.base_delay.max(h.delay.mul_f32(0.8));
                        }
                        stats.bytes += bytes;
                        if let Some(r) = redirected_from {
                            fr.seen.insert(r.as_bytes());
                        }
                        fr.seen.insert(record.url.as_bytes());
                        if let Some(c) = &record.canonical {
                            fr.seen.insert(c.as_bytes());
                        }
                        for l in &record.links {
                            self.enqueue(fr, &l.url, depth + 1);
                        }
                        writer.append(&record)?;
                        stats.stored += 1;
                    }
                    Ok(FetchResult::Skipped {
                        status,
                        retry_after,
                    }) => {
                        stats.fetched += 1;
                        stats.skipped += 1;
                        if status == 429 || status == 503 {
                            if let Some(h) = fr.hosts.get_mut(&origin) {
                                let backoff = (h.delay * 2).max(Duration::from_millis(500));
                                let wanted = retry_after
                                    .map(|s| Duration::from_secs(s.min(300)))
                                    .unwrap_or(backoff);
                                h.delay = wanted.min(Duration::from_secs(300));
                                next = now + h.delay;
                                h.next_ok = next;
                                log::debug!("{origin}: {status}, Pause jetzt {:?}", h.delay);
                            }
                        }
                    }
                    Err(e) => {
                        stats.errors += 1;
                        log::debug!("Fehler bei {origin}: {e:#}");
                    }
                }
                if fr.hosts.get(&origin).is_some_and(|h| !h.queue.is_empty()) {
                    fr.schedule(&origin, next);
                }
            }
        }
        Ok(())
    }

    fn save_frontier(&self, fr: &Frontier) -> Result<()> {
        let path = self.frontier_path();
        let tmp = path.with_extension("tmp");
        let mut w = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        let mut n = 0;
        for h in fr.hosts.values() {
            for (u, d) in &h.queue {
                writeln!(w, "{d} {u}")?;
                n += 1;
            }
        }
        w.flush()?;
        drop(w);
        std::fs::rename(&tmp, &path)?;
        if n > 0 {
            log::info!("{n} offene URLs in {} gespeichert", path.display());
        }
        Ok(())
    }

    async fn fetch_robots(&self, origin: &Url) -> Robots {
        let Ok(u) = origin.join("/robots.txt") else {
            return Robots::allow_all();
        };
        match self.client.get(u).send().await {
            Ok(resp) => {
                let st = resp.status().as_u16();
                if (200..300).contains(&st) {
                    match read_limited(resp, 512 * 1024).await {
                        Ok((b, _)) => {
                            Robots::parse(&String::from_utf8_lossy(&b), &self.agent_token)
                        }
                        Err(_) => Robots::allow_all(),
                    }
                } else if st == 401 || st == 403 {
                    Robots::deny_all()
                } else {
                    Robots::allow_all()
                }
            }
            Err(_) => Robots::allow_all(),
        }
    }

    async fn fetch_sitemaps(&self, sitemaps: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        let mut todo: VecDeque<String> = sitemaps.iter().take(3).cloned().collect();
        let mut fetched = 0;
        while let Some(sm) = todo.pop_front() {
            if fetched >= 6 || out.len() >= 20_000 {
                break;
            }
            fetched += 1;
            if sm.ends_with(".gz") {
                continue;
            }
            let Ok(resp) = self.client.get(&sm).send().await else {
                continue;
            };
            if !resp.status().is_success() {
                continue;
            }
            let Ok((body, _)) = read_limited(resp, 10 << 20).await else {
                continue;
            };
            let text = String::from_utf8_lossy(&body);
            let is_index = text.contains("<sitemapindex");
            for loc in xml_locs(&text) {
                if is_index {
                    if todo.len() < 10 {
                        todo.push_back(loc);
                    }
                } else if out.len() < 20_000 {
                    out.push(loc);
                }
            }
        }
        out
    }

    async fn fetch_page(&self, url: &str) -> Result<FetchResult> {
        let resp = self
            .client
            .get(url)
            .header("Accept", "text/html,application/xhtml+xml;q=0.9,*/*;q=0.1")
            .header("Accept-Language", "de,en;q=0.8,*;q=0.5")
            .send()
            .await?;
        let status = resp.status().as_u16();
        let final_url = urlnorm::normalize(resp.url()).context("ungültige Ziel-URL")?;
        if status != 200 {
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok());
            return Ok(FetchResult::Skipped {
                status,
                retry_after,
            });
        }
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        if let Some(ct) = &ct {
            let l = ct.to_ascii_lowercase();
            if !(l.contains("text/html") || l.contains("xhtml")) {
                return Ok(FetchResult::Skipped {
                    status: 0,
                    retry_after: None,
                });
            }
        }
        let robots_tag = resp
            .headers()
            .get("x-robots-tag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        let (body, _truncated) = read_limited(resp, self.cfg.max_body_bytes).await?;
        let bytes = body.len() as u64;
        let fu = final_url.clone();
        let mut record = tokio::task::spawn_blocking(move || {
            let html = html::decode_body(&body, ct.as_deref());
            let ex = html::extract(&html, &fu);
            let sample = format!(
                "{} {} {}",
                ex.title,
                ex.description,
                prefix(&ex.body, 20_000)
            );
            let lang = text::detect_lang(&sample, ex.lang_hint.as_deref());
            PageRecord {
                url: fu.to_string(),
                fetched_at: now_secs(),
                status,
                lang: lang as u8,
                noindex: ex.noindex,
                canonical: ex.canonical,
                title: ex.title,
                description: ex.description,
                headings: ex.headings,
                body: ex.body,
                links: ex.links,
            }
        })
        .await?;
        if robots_tag.contains("noindex") || robots_tag.contains("none") {
            record.noindex = true;
        }
        if robots_tag.contains("nofollow") {
            record.links.clear();
        }
        let redirected_from = (record.url != url).then(|| url.to_string());
        Ok(FetchResult::Html {
            record,
            bytes,
            redirected_from,
        })
    }
}

/// Wartet auf Strg+C oder SIGTERM.
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(t) => t,
                Err(_) => {
                    let _ = tokio::signal::ctrl_c().await;
                    return;
                }
            };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Die ersten höchstens `n` Bytes von `s`, an einer Zeichengrenze abgeschnitten.
pub fn prefix(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut end = n;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Liest den Body höchstens bis `limit` Bytes.
async fn read_limited(mut resp: reqwest::Response, limit: usize) -> Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        let room = limit.saturating_sub(buf.len());
        if chunk.len() >= room {
            buf.extend_from_slice(&chunk[..room]);
            return Ok((buf, true));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok((buf, false))
}

/// Extrahiert `<loc>`-Einträge aus Sitemap-XML.
fn xml_locs(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<loc>") {
        rest = &rest[i + 5..];
        let Some(j) = rest.find("</loc>") else { break };
        let v = rest[..j]
            .trim()
            .trim_start_matches("<![CDATA[")
            .trim_end_matches("]]>")
            .trim();
        out.push(v.replace("&amp;", "&"));
        rest = &rest[j + 6..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sitemap() {
        let xml = "<urlset><url><loc> https://a.de/x?a=1&amp;b=2 </loc></url><url><loc><![CDATA[https://a.de/y]]></loc></url></urlset>";
        assert_eq!(
            xml_locs(xml),
            vec!["https://a.de/x?a=1&b=2", "https://a.de/y"]
        );
    }
}
