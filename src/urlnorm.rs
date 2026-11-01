//! URL-Normalisierung und Sortierschlüssel.

use url::Url;

const TRACKING_PARAMS: &[&str] = &[
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_term",
    "utm_content",
    "gclid",
    "fbclid",
    "mc_cid",
    "mc_eid",
    "sessionid",
    "phpsessid",
    "sid",
    "ref_src",
    "igshid",
    "yclid",
    "_hsenc",
    "_hsmi",
];

const SKIP_EXT: &[&str] = &[
    ".jpg", ".jpeg", ".png", ".gif", ".webp", ".svg", ".ico", ".bmp", ".tif", ".tiff", ".pdf",
    ".zip", ".gz", ".tgz", ".rar", ".7z", ".tar", ".bz2", ".xz", ".exe", ".dmg", ".msi", ".iso",
    ".apk", ".mp3", ".mp4", ".avi", ".mov", ".mkv", ".webm", ".wav", ".ogg", ".flac", ".m4a",
    ".css", ".js", ".json", ".xml", ".rss", ".atom", ".woff", ".woff2", ".ttf", ".otf", ".eot",
    ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx", ".odt", ".bin", ".deb", ".rpm",
];

/// Normalisiert eine absolute URL: nur http(s), kleingeschriebener Host, ohne Fragment,
/// ohne Standardport, ohne Tracking-Parameter. `None` für nicht crawlbare URLs.
pub fn normalize(u: &Url) -> Option<Url> {
    if u.scheme() != "http" && u.scheme() != "https" {
        return None;
    }
    u.host_str()?;
    let mut u = u.clone();
    u.set_fragment(None);
    if u.username() != "" || u.password().is_some() {
        let _ = u.set_username("");
        let _ = u.set_password(None);
    }
    if u.query().is_some() {
        let pairs: Vec<(String, String)> = u
            .query_pairs()
            .filter(|(k, _)| {
                let k = k.to_ascii_lowercase();
                !k.starts_with("utm_") && !TRACKING_PARAMS.contains(&k.as_str())
            })
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        if pairs.is_empty() {
            u.set_query(None);
        } else {
            u.query_pairs_mut().clear().extend_pairs(pairs);
        }
    }
    if u.as_str().len() > 1024 {
        return None;
    }
    Some(u)
}

/// Löst `href` relativ zu `base` auf und normalisiert.
pub fn resolve(base: &Url, href: &str) -> Option<Url> {
    let href = href.trim();
    if href.is_empty()
        || href.starts_with('#')
        || href.starts_with("javascript:")
        || href.starts_with("mailto:")
        || href.starts_with("tel:")
        || href.starts_with("data:")
    {
        return None;
    }
    let u = base.join(href).ok()?;
    normalize(&u)
}

/// Lohnt es sich, die URL zu laden (HTML erwartet)?
pub fn looks_crawlable(u: &Url) -> bool {
    let path = u.path().to_ascii_lowercase();
    !SKIP_EXT.iter().any(|e| path.ends_with(e))
}

/// Host ohne führendes `www.`.
pub fn host_key(u: &Url) -> String {
    let h = u.host_str().unwrap_or("").to_ascii_lowercase();
    h.strip_prefix("www.").map(str::to_string).unwrap_or(h)
}

/// Umgekehrte Host-Labels: `www.example.com` → `com.example.www`.
pub fn reverse_host(host: &str) -> String {
    let mut labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    labels.reverse();
    labels.join(".")
}

/// Sortierschlüssel für Dokument-IDs: umgekehrter Host + Pfad. Dadurch liegen alle
/// Seiten einer Domain (inkl. Subdomains) in einem zusammenhängenden ID-Bereich –
/// gut für die Delta-Kompression und für `site:`-Filter.
pub fn sort_key(u: &Url) -> String {
    let mut k = reverse_host(&u.host_str().unwrap_or("").to_ascii_lowercase());
    k.push_str(&u[url::Position::BeforePath..]);
    k
}

/// Pfad-Tiefe (Anzahl nicht-leerer Segmente), Query zählt als zusätzliche Ebene.
pub fn depth(u: &Url) -> usize {
    let d = u.path().split('/').filter(|s| !s.is_empty()).count();
    d + usize::from(u.query().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes() {
        let u = Url::parse("HTTP://User:pw@Example.COM:80/a/b?utm_source=x&q=1#frag").unwrap();
        let n = normalize(&u).unwrap();
        assert_eq!(n.as_str(), "http://example.com/a/b?q=1");
        let u = Url::parse("https://example.com/?utm_medium=a").unwrap();
        assert_eq!(normalize(&u).unwrap().as_str(), "https://example.com/");
        assert!(normalize(&Url::parse("ftp://example.com/").unwrap()).is_none());
    }

    #[test]
    fn resolves() {
        let base = Url::parse("https://example.com/dir/page.html").unwrap();
        assert_eq!(
            resolve(&base, "../x").unwrap().as_str(),
            "https://example.com/x"
        );
        assert!(resolve(&base, "javascript:void(0)").is_none());
        assert!(resolve(&base, "#top").is_none());
    }

    #[test]
    fn keys() {
        let u = Url::parse("https://www.example.com/a?b=1").unwrap();
        assert_eq!(sort_key(&u), "com.example.www/a?b=1");
        assert_eq!(host_key(&u), "example.com");
        assert_eq!(depth(&u), 2);
        assert!(!looks_crawlable(
            &Url::parse("https://x.de/bild.JPG").unwrap()
        ));
    }
}
