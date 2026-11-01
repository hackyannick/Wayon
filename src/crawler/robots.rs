//! robots.txt-Parser (RFC 9309): User-agent-Gruppen, Allow/Disallow mit `*` und `$`,
//! längste Regel gewinnt, Crawl-delay und Sitemap-Einträge.

#[derive(Clone, Debug, Default)]
pub struct Robots {
    rules: Vec<(bool, String)>,
    pub crawl_delay: Option<f64>,
    pub sitemaps: Vec<String>,
}

impl Robots {
    /// Alles erlaubt (z.B. wenn robots.txt fehlt).
    pub fn allow_all() -> Robots {
        Robots::default()
    }

    /// Alles verboten (z.B. bei 401/403 auf robots.txt).
    pub fn deny_all() -> Robots {
        Robots {
            rules: vec![(false, "/".into())],
            ..Default::default()
        }
    }

    pub fn parse(text: &str, agent_token: &str) -> Robots {
        let agent = agent_token.to_ascii_lowercase();
        // (agents, rules, crawl_delay)
        let mut groups: Vec<(Vec<String>, Vec<(bool, String)>, Option<f64>)> = Vec::new();
        let mut sitemaps = Vec::new();
        let mut last_was_agent = false;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((key, val)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let val = val.trim();
            match key.as_str() {
                "user-agent" => {
                    if !last_was_agent || groups.is_empty() {
                        groups.push((Vec::new(), Vec::new(), None));
                    }
                    groups.last_mut().unwrap().0.push(val.to_ascii_lowercase());
                    last_was_agent = true;
                }
                "allow" | "disallow" => {
                    last_was_agent = false;
                    if let Some(g) = groups.last_mut() {
                        if !val.is_empty() {
                            g.1.push((key == "allow", val.to_string()));
                        }
                    }
                }
                "crawl-delay" => {
                    last_was_agent = false;
                    if let (Some(g), Ok(d)) = (groups.last_mut(), val.parse::<f64>()) {
                        g.2 = Some(d);
                    }
                }
                "sitemap" => {
                    // `val` enthält nach split_once evtl. noch "//..." – Originalzeile nutzen.
                    let full = line[line.find(':').unwrap() + 1..].trim();
                    if !full.is_empty() {
                        sitemaps.push(full.to_string());
                    }
                }
                _ => {}
            }
        }
        let specific: Vec<_> = groups
            .iter()
            .filter(|g| g.0.iter().any(|a| a != "*" && agent.contains(a.as_str())))
            .collect();
        let chosen: Vec<_> = if specific.is_empty() {
            groups
                .iter()
                .filter(|g| g.0.iter().any(|a| a == "*"))
                .collect()
        } else {
            specific
        };
        let mut r = Robots {
            sitemaps,
            ..Default::default()
        };
        for g in chosen {
            r.rules.extend(g.1.iter().cloned());
            if g.2.is_some() {
                r.crawl_delay = g.2;
            }
        }
        r
    }

    /// Prüft Pfad+Query (z.B. `/a/b?c=1`).
    pub fn is_allowed(&self, path: &str) -> bool {
        let mut best: Option<(usize, bool)> = None;
        for (allow, pat) in &self.rules {
            if glob_match(pat, path) {
                let len = pat.len();
                best = match best {
                    None => Some((len, *allow)),
                    Some((bl, ba)) if len > bl || (len == bl && *allow && !ba) => {
                        Some((len, *allow))
                    }
                    b => b,
                };
            }
        }
        best.map(|b| b.1).unwrap_or(true)
    }
}

/// Präfix-Match mit `*` (beliebig viele Zeichen) und `$` (Ende).
fn glob_match(pat: &str, text: &str) -> bool {
    let (pat, anchored) = match pat.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pat, false),
    };
    let p = pat.as_bytes();
    let t = text.as_bytes();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    loop {
        if pi == p.len() {
            if !anchored || ti == t.len() {
                return true;
            }
        } else if p[pi] == b'*' {
            star = Some((pi, ti));
            pi += 1;
            continue;
        } else if ti < t.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
            continue;
        }
        // Backtracking zum letzten `*`
        match star {
            Some((sp, st)) if st < t.len() => {
                star = Some((sp, st + 1));
                pi = sp + 1;
                ti = st + 1;
            }
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TXT: &str = "
User-agent: *
Disallow: /private
Allow: /private/public
Disallow: /*.php$
Crawl-delay: 2

User-agent: WayonBot
User-agent: OtherBot
Disallow: /nowayon
Sitemap: https://example.com/sitemap.xml
";

    #[test]
    fn generic_group() {
        let r = Robots::parse(TXT, "SomeBot");
        assert!(!r.is_allowed("/private/x"));
        assert!(r.is_allowed("/private/public/y"));
        assert!(!r.is_allowed("/index.php"));
        assert!(r.is_allowed("/index.php?x=1"));
        assert!(r.is_allowed("/"));
        assert_eq!(r.crawl_delay, Some(2.0));
        assert_eq!(r.sitemaps, vec!["https://example.com/sitemap.xml"]);
    }

    #[test]
    fn specific_group() {
        let r = Robots::parse(TXT, "WayonBot");
        assert!(r.is_allowed("/private/x"));
        assert!(!r.is_allowed("/nowayon/a"));
    }

    #[test]
    fn globbing() {
        assert!(glob_match("/a*b", "/axxb/c"));
        assert!(glob_match("/a*b$", "/axxb"));
        assert!(!glob_match("/a*b$", "/axxbc"));
        assert!(glob_match("*", "/anything"));
        assert!(!Robots::deny_all().is_allowed("/"));
    }
}
