//! Minimaler, streamender WARC-Leser (ISO 28500) für Common-Crawl-Dateien.
//! Liefert die HTTP-Antworten (`WARC-Type: response`) mit Status, Headern und Body.

use anyhow::{Context, Result};
use std::io::{BufRead, Read};

pub struct HttpResponse {
    pub url: String,
    pub date: String,
    pub status: u16,
    pub content_type: Option<String>,
    pub robots_tag: String,
    pub body: Vec<u8>,
}

pub struct WarcReader<R: BufRead> {
    r: R,
    line: Vec<u8>,
}

impl<R: BufRead> WarcReader<R> {
    pub fn new(r: R) -> Self {
        WarcReader {
            r,
            line: Vec::new(),
        }
    }

    /// Nächste HTTP-Antwort; andere Record-Typen (request, metadata …) werden übersprungen.
    pub fn next_response(&mut self) -> Result<Option<HttpResponse>> {
        loop {
            // Header-Block: erste nichtleere Zeile muss "WARC/x.y" sein
            let mut first = true;
            let (mut wtype, mut uri, mut date, mut len) =
                (String::new(), String::new(), String::new(), None);
            loop {
                self.line.clear();
                if self.r.read_until(b'\n', &mut self.line)? == 0 {
                    return Ok(None);
                }
                let l = String::from_utf8_lossy(&self.line);
                let l = l.trim_end_matches(['\r', '\n']);
                if first {
                    if l.is_empty() {
                        continue; // Leerzeilen zwischen Records
                    }
                    anyhow::ensure!(l.starts_with("WARC/"), "kein WARC-Record: {l:.40}");
                    first = false;
                    continue;
                }
                if l.is_empty() {
                    break;
                }
                if let Some((k, v)) = l.split_once(':') {
                    let v = v.trim();
                    match k.trim().to_ascii_lowercase().as_str() {
                        "warc-type" => wtype = v.to_string(),
                        "warc-target-uri" => uri = v.trim_matches(['<', '>']).to_string(),
                        "warc-date" => date = v.to_string(),
                        "content-length" => len = v.parse::<u64>().ok(),
                        _ => {}
                    }
                }
            }
            let len = len.context("WARC-Record ohne Content-Length")?;
            if wtype != "response" || len > 64 << 20 {
                std::io::copy(&mut (&mut self.r).take(len), &mut std::io::sink())?;
                continue;
            }
            let mut payload = vec![0u8; len as usize];
            self.r.read_exact(&mut payload)?;
            if let Some(resp) = parse_http(uri, date, payload) {
                return Ok(Some(resp));
            }
        }
    }
}

fn find_header_end(p: &[u8]) -> Option<(usize, usize)> {
    if let Some(i) = p.windows(4).position(|w| w == b"\r\n\r\n") {
        return Some((i, i + 4));
    }
    p.windows(2).position(|w| w == b"\n\n").map(|i| (i, i + 2))
}

fn parse_http(url: String, date: String, payload: Vec<u8>) -> Option<HttpResponse> {
    let (hend, bstart) = find_header_end(&payload)?;
    let head = String::from_utf8_lossy(&payload[..hend]);
    let mut lines = head.lines();
    let status_line = lines.next()?;
    if !status_line.starts_with("HTTP/") {
        return None;
    }
    let status = status_line.split_whitespace().nth(1)?.parse().ok()?;
    let (mut ct, mut robots, mut chunked, mut gzip) = (None, String::new(), false, false);
    for l in lines {
        let Some((k, v)) = l.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "content-type" => ct = Some(v.to_string()),
            "x-robots-tag" => robots = v.to_string(),
            // Common Crawl speichert den Body bereits dekodiert und benennt diese
            // Header in X-Crawler-* um; nur echte Kodierungen behandeln.
            "transfer-encoding" => chunked = v.to_ascii_lowercase().contains("chunked"),
            "content-encoding" => gzip = v.to_ascii_lowercase().contains("gzip"),
            _ => {}
        }
    }
    let mut body = payload[bstart..].to_vec();
    if chunked {
        body = dechunk(&body).unwrap_or(body);
    }
    if gzip {
        let mut out = Vec::new();
        if flate2::read::MultiGzDecoder::new(&body[..])
            .read_to_end(&mut out)
            .is_ok()
        {
            body = out;
        }
    }
    Some(HttpResponse {
        url,
        date,
        status,
        content_type: ct,
        robots_tag: robots,
        body,
    })
}

fn dechunk(b: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(b.len());
    let mut p = 0;
    loop {
        let nl = b[p..].iter().position(|&c| c == b'\n')? + p;
        let size_str = std::str::from_utf8(&b[p..nl]).ok()?.trim();
        let size = usize::from_str_radix(size_str.split(';').next()?.trim(), 16).ok()?;
        p = nl + 1;
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(b.get(p..p + size)?);
        p += size;
        // CRLF nach dem Chunk
        while p < b.len() && (b[p] == b'\r' || b[p] == b'\n') {
            p += 1;
            if b.get(p - 1) == Some(&b'\n') {
                break;
            }
        }
    }
}

/// WARC-Datum (`2025-11-14T03:12:55Z`) in Unix-Sekunden (grob, ohne chrono).
pub fn warc_date_secs(d: &str) -> u64 {
    let n: Vec<u64> = d
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    if n.len() < 6 {
        return crate::crawler::now_secs();
    }
    let (y, m, day) = (n[0] as i64, n[1] as i64, n[2] as i64);
    // Tage seit 1970 (Algorithmus von Howard Hinnant)
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    (days * 86_400 + (n[3] * 3600 + n[4] * 60 + n[5]) as i64).max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(wtype: &str, uri: &str, payload: &[u8]) -> Vec<u8> {
        let mut r = format!(
            "WARC/1.0\r\nWARC-Type: {wtype}\r\nWARC-Target-URI: {uri}\r\nWARC-Date: 2025-11-14T03:12:55Z\r\nContent-Length: {}\r\n\r\n",
            payload.len()
        )
        .into_bytes();
        r.extend_from_slice(payload);
        r.extend_from_slice(b"\r\n\r\n");
        r
    }

    #[test]
    fn parses_records() {
        let mut data = record("warcinfo", "", b"software: test");
        data.extend(record(
            "request",
            "https://a.de/",
            b"GET / HTTP/1.1\r\n\r\n",
        ));
        data.extend(record(
            "response",
            "https://a.de/",
            b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nX-Robots-Tag: noindex\r\n\r\n<p>Hallo</p>",
        ));
        data.extend(record(
            "response",
            "https://b.de/",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nHallo\r\n6\r\n Welt!\r\n0\r\n\r\n",
        ));
        let mut r = WarcReader::new(&data[..]);
        let a = r.next_response().unwrap().unwrap();
        assert_eq!(a.url, "https://a.de/");
        assert_eq!(a.status, 200);
        assert_eq!(a.body, b"<p>Hallo</p>");
        assert_eq!(a.robots_tag, "noindex");
        assert!(a.content_type.unwrap().contains("text/html"));
        let b = r.next_response().unwrap().unwrap();
        assert_eq!(b.body, b"Hallo Welt!");
        assert!(r.next_response().unwrap().is_none());
    }

    #[test]
    fn dates() {
        assert_eq!(warc_date_secs("1970-01-01T00:00:00Z"), 0);
        assert_eq!(warc_date_secs("2025-11-14T03:12:55Z"), 1_763_089_975);
    }
}
