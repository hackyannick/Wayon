//! Erzeugt einen synthetischen Crawl-Speicher für Last- und Größentests.
//!
//! cargo run --release --example synth -- <datenverzeichnis> <anzahl_seiten>
//! wayon --data <datenverzeichnis> index && wayon --data <datenverzeichnis> stats

use wayon::store::{Link, PageRecord, StoreWriter};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    /// Zipf-ähnliche Verteilung über 0..n (häufige Wörter sind sehr häufig).
    fn zipf(&mut self, n: u64) -> u64 {
        let u = (self.next() >> 11) as f64 / (1u64 << 53) as f64;
        ((n as f64).powf(u) - 1.0) as u64
    }
}

fn word(i: u64) -> String {
    const SYL: [&str; 16] = [
        "ka", "lo", "mi", "ren", "sto", "va", "ber", "tin", "qua", "fel", "dor", "un", "zei",
        "pra", "chi", "mos",
    ];
    let mut s = String::new();
    let mut x = i + 1;
    while x > 0 {
        s.push_str(SYL[(x % 16) as usize]);
        x /= 16;
    }
    s
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let dir = std::path::PathBuf::from(args.get(1).map(String::as_str).unwrap_or("synth"));
    let n: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let vocab = 200_000;
    let hosts = (n / 200).max(1);
    let mut rng = Rng(0x9E3779B97F4A7C15);
    let mut w = StoreWriter::open(&dir.join("crawl"))?;
    let url = |i: u64| format!("https://site{}.example/seite/{}", i % hosts, i);
    for i in 0..n {
        let title: Vec<String> = (0..6).map(|_| word(rng.zipf(vocab))).collect();
        let len = 150 + rng.below(600);
        let body: Vec<String> = (0..len).map(|_| word(rng.zipf(vocab))).collect();
        let links = (0..12)
            .map(|_| {
                let t = if rng.below(3) == 0 {
                    rng.below(n)
                } else {
                    (i / hosts) * hosts + rng.below(hosts) % n
                };
                Link {
                    url: url(t.min(n - 1)),
                    anchor: word(rng.zipf(vocab)),
                }
            })
            .collect();
        w.append(&PageRecord {
            url: url(i),
            fetched_at: 1,
            status: 200,
            lang: 0,
            noindex: false,
            canonical: None,
            title: title.join(" "),
            description: String::new(),
            headings: String::new(),
            body: body.join(" "),
            links,
        })?;
    }
    w.flush()?;
    println!("{n} synthetische Seiten in {} geschrieben", dir.display());
    Ok(())
}
