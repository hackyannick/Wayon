//! HTTP-Server: Web-Oberfläche, JSON-API, Autovervollständigung, OpenSearch.

use crate::index::Index;
use crate::search::snippet::html_escape;
use crate::search::{SearchResults, Searcher};
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use std::fmt::Write;
use std::net::SocketAddr;
use std::sync::Arc;

const PER_PAGE: usize = 10;

#[derive(Deserialize, Default)]
pub struct SearchParams {
    #[serde(default)]
    q: String,
    #[serde(default)]
    page: Option<usize>,
    #[serde(default)]
    n: Option<usize>,
}

type AppState = Arc<Index>;

pub fn router(index: Arc<Index>) -> Router {
    Router::new()
        .route("/", get(home))
        .route("/search", get(home))
        .route("/api/search", get(api_search))
        .route("/api/suggest", get(api_suggest))
        .route("/opensearch.xml", get(opensearch))
        .with_state(index)
}

pub async fn serve(index: Index, addr: SocketAddr) -> anyhow::Result<()> {
    let n = index.num_docs();
    let app = router(Arc::new(index));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    log::info!("Wayon läuft auf http://{addr}  ({n} Dokumente im Index)");
    axum::serve(listener, app)
        .with_graceful_shutdown(crate::crawler::shutdown_signal())
        .await?;
    Ok(())
}

async fn run_search(index: AppState, q: String, page: usize, n: usize) -> SearchResults {
    tokio::task::spawn_blocking(move || Searcher::new(&index).search(&q, page * n, n))
        .await
        .unwrap_or_default()
}

async fn api_search(
    State(index): State<AppState>,
    Query(p): Query<SearchParams>,
) -> Json<SearchResults> {
    let n = p.n.unwrap_or(PER_PAGE).clamp(1, 100);
    let page = p.page.unwrap_or(1).max(1) - 1;
    Json(run_search(index, p.q, page.min(99), n).await)
}

async fn api_suggest(
    State(index): State<AppState>,
    Query(p): Query<SearchParams>,
) -> Json<serde_json::Value> {
    let q = p.q.clone();
    let list = tokio::task::spawn_blocking(move || Searcher::new(&index).complete(&q, 8))
        .await
        .unwrap_or_default();
    Json(serde_json::json!([p.q, list]))
}

async fn opensearch() -> Response {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<OpenSearchDescription xmlns="http://a9.com/-/spec/opensearch/1.1/">
  <ShortName>Wayon</ShortName>
  <Description>Wayon Websuche</Description>
  <InputEncoding>UTF-8</InputEncoding>
  <Url type="text/html" template="/search?q={searchTerms}"/>
  <Url type="application/x-suggestions+json" template="/api/suggest?q={searchTerms}"/>
</OpenSearchDescription>"#;
    (
        [(
            header::CONTENT_TYPE,
            "application/opensearchdescription+xml",
        )],
        xml,
    )
        .into_response()
}

const STYLE: &str = r#"
*{box-sizing:border-box}body{margin:0;font-family:system-ui,-apple-system,"Segoe UI",Roboto,Arial,sans-serif;color:#202124;background:#fff}
a{color:#1a0dab;text-decoration:none}a:hover{text-decoration:underline}
.logo{font-weight:700;letter-spacing:-1px;color:#202124}.logo span:nth-child(1){color:#4285f4}.logo span:nth-child(2){color:#ea4335}.logo span:nth-child(3){color:#fbbc05}.logo span:nth-child(4){color:#4285f4}.logo span:nth-child(5){color:#34a853}
.home{display:flex;flex-direction:column;align-items:center;margin-top:22vh}.home .logo{font-size:84px;margin-bottom:28px}
form{position:relative}.box{display:flex;align-items:center;border:1px solid #dfe1e5;border-radius:24px;padding:0 16px;height:46px;width:min(584px,92vw);background:#fff}
.box:hover,.box:focus-within{box-shadow:0 1px 6px rgba(32,33,36,.28);border-color:transparent}
.box input{flex:1;border:0;outline:0;font-size:16px;height:100%;background:transparent}
.box button{border:0;background:none;cursor:pointer;font-size:18px;color:#4285f4}
#ac{position:absolute;top:46px;left:0;right:0;background:#fff;border-radius:0 0 24px 24px;box-shadow:0 4px 6px rgba(32,33,36,.28);display:none;z-index:9;padding-bottom:8px}
#ac div{padding:6px 18px;cursor:pointer}#ac div.sel,#ac div:hover{background:#eee}
header{display:flex;align-items:center;gap:28px;padding:20px 28px;border-bottom:1px solid #ebebeb}header .logo{font-size:30px}
main{max-width:652px;margin-left:min(180px,4vw);padding:12px 16px}
.stats{color:#70757a;font-size:14px;margin:4px 0 20px}.sugg{font-size:18px;margin:0 0 18px;color:#c5221f}.sugg a{font-style:italic;font-weight:600}
.r{margin-bottom:28px}.r .u{font-size:14px;color:#4d5156;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.r h3{font-size:20px;font-weight:400;margin:2px 0 4px;line-height:1.3}.r p{margin:0;font-size:14px;line-height:1.58;color:#4d5156}.r p b{color:#202124}
.pg{display:flex;gap:14px;margin:30px 0 60px;font-size:15px}.pg .cur{font-weight:700;color:#202124}
footer{color:#70757a;font-size:13px;margin-top:40px;text-align:center}
"#;

const SCRIPT: &str = r#"
(function(){const i=document.getElementById('q'),ac=document.getElementById('ac');if(!i)return;let sel=-1,items=[],t;
function show(){ac.innerHTML='';items.forEach((s,k)=>{const d=document.createElement('div');d.textContent=s;if(k===sel)d.className='sel';d.onmousedown=e=>{e.preventDefault();i.value=s;i.form.submit()};ac.appendChild(d)});ac.style.display=items.length?'block':'none'}
i.addEventListener('input',()=>{clearTimeout(t);t=setTimeout(async()=>{const v=i.value;if(!v.trim()){items=[];show();return}try{const r=await fetch('/api/suggest?q='+encodeURIComponent(v));const j=await r.json();items=j[1]||[];sel=-1;show()}catch(e){}},90)});
i.addEventListener('keydown',e=>{if(!items.length)return;if(e.key==='ArrowDown'){sel=(sel+1)%items.length;i.value=items[sel];show();e.preventDefault()}else if(e.key==='ArrowUp'){sel=(sel-1+items.length)%items.length;i.value=items[sel];show();e.preventDefault()}else if(e.key==='Escape'){items=[];show()}});
i.addEventListener('blur',()=>setTimeout(()=>{items=[];show()},100));})();
"#;

fn logo() -> &'static str {
    "<span>W</span><span>a</span><span>y</span><span>o</span><span>n</span>"
}

fn search_form(q: &str, autofocus: bool) -> String {
    format!(
        r#"<form action="/search" method="get" autocomplete="off"><div class="box"><input id="q" name="q" value="{}" {} aria-label="Suche"><button type="submit" aria-label="Suchen">&#128269;</button></div><div id="ac"></div></form>"#,
        html_escape(q),
        if autofocus { "autofocus" } else { "" }
    )
}

fn page(title: &str, body: &str) -> Html<String> {
    Html(format!(
        r#"<!DOCTYPE html><html lang="de"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{}</title><link rel="search" type="application/opensearchdescription+xml" title="Wayon" href="/opensearch.xml"><style>{STYLE}</style></head><body>{body}<script>{SCRIPT}</script></body></html>"#,
        html_escape(title)
    ))
}

/// „example.com › pfad › seite“
fn display_url(u: &str) -> String {
    let Ok(p) = url::Url::parse(u) else {
        return u.to_string();
    };
    let host = p.host_str().unwrap_or("").trim_start_matches("www.");
    let segs: Vec<String> = p
        .path_segments()
        .map(|s| {
            s.filter(|x| !x.is_empty())
                .take(4)
                .map(percent_decode)
                .collect()
        })
        .unwrap_or_default();
    if segs.is_empty() {
        host.to_string()
    } else {
        format!("{host} › {}", segs.join(" › "))
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn q_link(q: &str, page: usize) -> String {
    let enc: String = url::form_urlencoded::byte_serialize(q.as_bytes()).collect();
    if page <= 1 {
        format!("/search?q={enc}")
    } else {
        format!("/search?q={enc}&page={page}")
    }
}

async fn home(State(index): State<AppState>, Query(p): Query<SearchParams>) -> Html<String> {
    if p.q.trim().is_empty() {
        let body = format!(
            r#"<div class="home"><div class="logo">{}</div>{}<footer>{} Seiten im Index</footer></div>"#,
            logo(),
            search_form("", true),
            fmt_num(index.num_docs() as u64)
        );
        return page("Wayon", &body);
    }
    let cur = p.page.unwrap_or(1).clamp(1, 100);
    let q = p.q.clone();
    let res = run_search(index, q.clone(), cur - 1, PER_PAGE).await;
    let mut b = String::new();
    let _ = write!(
        b,
        r#"<header><a class="logo" href="/">{}</a>{}</header><main>"#,
        logo(),
        search_form(&q, false)
    );
    let _ = write!(
        b,
        r#"<div class="stats">Ungefähr {} Ergebnisse ({:.3} Sekunden)</div>"#,
        fmt_num(res.total),
        res.took_ms / 1000.0
    );
    if let Some(c) = &res.corrected {
        let _ = write!(
            b,
            r#"<p class="sugg">Ergebnisse für <a href="{}">{}</a> werden angezeigt</p>"#,
            q_link(c, 1),
            html_escape(c)
        );
    } else if let Some(s) = &res.suggestion {
        let _ = write!(
            b,
            r#"<p class="sugg">Meinten Sie: <a href="{}">{}</a></p>"#,
            q_link(s, 1),
            html_escape(s)
        );
    }
    if res.hits.is_empty() {
        let _ = write!(
            b,
            "<p>Die Suche nach <b>{}</b> ergab keine Treffer.</p><ul><li>Prüfen Sie die Schreibweise.</li><li>Versuchen Sie andere oder allgemeinere Suchbegriffe.</li></ul>",
            html_escape(&q)
        );
    }
    for h in &res.hits {
        let _ = write!(
            b,
            r#"<div class="r"><div class="u">{}</div><h3><a href="{}">{}</a></h3><p>{}</p></div>"#,
            html_escape(&display_url(&h.url)),
            html_escape(&h.url),
            html_escape(&h.title),
            h.snippet
        );
    }
    let pages = res.total.div_ceil(PER_PAGE as u64).min(30) as usize;
    if pages > 1 {
        b.push_str(r#"<div class="pg">"#);
        if cur > 1 {
            let _ = write!(b, r#"<a href="{}">‹ Zurück</a>"#, q_link(&q, cur - 1));
        }
        let start = cur.saturating_sub(5).max(1);
        for pn in start..=(start + 9).min(pages) {
            if pn == cur {
                let _ = write!(b, r#"<span class="cur">{pn}</span>"#);
            } else {
                let _ = write!(b, r#"<a href="{}">{pn}</a>"#, q_link(&q, pn));
            }
        }
        if cur < pages {
            let _ = write!(b, r#"<a href="{}">Weiter ›</a>"#, q_link(&q, cur + 1));
        }
        b.push_str("</div>");
    }
    b.push_str("</main>");
    page(&format!("{q} – Wayon"), &b)
}

fn fmt_num(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push('.');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(fmt_num(1234567), "1.234.567");
        assert_eq!(fmt_num(12), "12");
        assert_eq!(
            display_url("https://www.example.com/a/b%C3%BC/"),
            "example.com › a › bü"
        );
        assert_eq!(display_url("https://example.com/"), "example.com");
    }
}
