//! Web research tools: multi-provider `websearch` + redirect-checked `webfetch`.
//! Mirrors Go `doWebSearch`/`doWebFetch` (tavily, exa, firecrawl, parallel,
//! tinyfish; keys from env; per-provider 429 cooldown; 100KB fetch cap).

use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WebError {
    #[error("{0}")]
    Msg(String),
    #[error("websearch: provider rate limited (429)")]
    Limited(String),
}

pub fn providers() -> &'static [&'static str] {
    &["tavily", "exa", "firecrawl", "parallel", "tinyfish"]
}

pub fn key_for(provider: &str) -> String {
    let var = match provider {
        "tavily" => "TAVILY_API_KEY",
        "exa" => "EXA_API_KEY",
        "firecrawl" => "FIRECRAWL_API_KEY",
        "parallel" => "PARALLEL_API_KEY",
        "tinyfish" => "TINYFISH_API_KEY",
        _ => return String::new(),
    };
    std::env::var(var).unwrap_or_default()
}

fn cooldowns() -> &'static Mutex<HashMap<String, Instant>> {
    static ONCE: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    ONCE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cooling(provider: &str) -> bool {
    cooldowns().lock().unwrap().get(provider).is_some_and(|&u| Instant::now() < u)
}

fn backoff(provider: &str, retry_after: &str) {
    let mut secs: u64 = retry_after.trim().parse().unwrap_or(60);
    secs = secs.clamp(1, 3600);
    cooldowns().lock().unwrap().insert(provider.into(), Instant::now() + Duration::from_secs(secs));
}

fn blocking_client(timeout: Duration) -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .user_agent("lumina/0.1.0")
        .timeout(timeout)
        .build()
        .expect("blocking http client")
}

fn post_json(url: &str, key: &str, body: &serde_json::Value, api_key_header: Option<(&str, &str)>) -> Result<serde_json::Value, WebError> {
    let mut req = blocking_client(Duration::from_secs(30)).post(url).json(body);
    if let Some((h, v)) = api_key_header {
        req = req.header(h, v);
    } else if !key.is_empty() {
        req = req.bearer_auth(key);
    }
    let res = req.send().map_err(|e| WebError::Msg(format!("websearch: {e}")))?;
    if res.status().as_u16() == 429 {
        let ra = res.headers().get("retry-after").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        return Err(WebError::Limited(ra));
    }
    if !res.status().is_success() {
        let st = res.status();
        let t = res.text().unwrap_or_default();
        return Err(WebError::Msg(format!("websearch: status {st}: {}", trunc(&t, 300))));
    }
    res.json().map_err(|e| WebError::Msg(format!("websearch: bad JSON: {e}")))
}

fn trunc(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    format!("{}…", &s[..n])
}

fn run_one(provider: &str, key: &str, query: &str) -> Result<String, WebError> {
    if key.is_empty() {
        return Err(WebError::Msg(format!("websearch: {provider} key is not set")));
    }
    match provider {
        "tavily" => {
            let v = post_json(
                "https://api.tavily.com/search",
                key,
                &serde_json::json!({"query": query, "search_depth": "basic", "max_results": 8, "include_answer": false}),
                None,
            )?;
            Ok(fmt_results(&v, "results", &["title", "url", "content"]))
        }
        "exa" => {
            let v = post_json(
                "https://api.exa.ai/search",
                key,
                &serde_json::json!({"query": query, "type": "auto", "numResults": 8}),
                None,
            )?;
            Ok(fmt_results(&v, "results", &["title", "url", "text"]))
        }
        "firecrawl" => {
            let v = post_json(
                "https://api.firecrawl.dev/v2/search",
                key,
                &serde_json::json!({"query": query, "limit": 8}),
                None,
            )?;
            Ok(fmt_results(&v, "data", &["title", "url", "markdown"]))
        }
        "parallel" => {
            let v = post_json(
                "https://api.parallel.ai/v1/search",
                "",
                &serde_json::json!({"objective": query, "search_queries": [query], "mode": "basic"}),
                Some(("x-api-key", key)),
            )?;
            Ok(fmt_results(&v, "results", &["title", "url", "content"]))
        }
        "tinyfish" => {
            let res = blocking_client(Duration::from_secs(30))
                .get(format!("https://api.search.tinyfish.ai?query={}", url_encode(query)))
                .header("X-API-Key", key)
                .send()
                .map_err(|e| WebError::Msg(format!("websearch: {e}")))?;
            if res.status().as_u16() == 429 {
                return Err(WebError::Msg("websearch: provider rate limited (429)".into()));
            }
            if !res.status().is_success() {
                return Err(WebError::Msg(format!("websearch: status {}", res.status())));
            }
            let v: serde_json::Value = res.json().map_err(|e| WebError::Msg(format!("websearch: bad JSON: {e}")))?;
            Ok(fmt_results(&v, "results", &["title", "url", "snippet"]))
        }
        _ => Err(WebError::Msg(format!("websearch: unknown provider {provider:?}"))),
    }
}

fn url_encode(s: &str) -> String {
    // ponytail: minimal percent-encoding for query params (no url crate).
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b" -_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out.replace(' ', "+")
}

fn fmt_results(v: &serde_json::Value, arr_key: &str, fields: &[&str]) -> String {
    let mut sb = String::new();
    let arr = v.get(arr_key).and_then(|a| a.as_array()).cloned().unwrap_or_default();
    // Fall back to a top-level list when the provider shape differs.
    let arr = if arr.is_empty() { v.as_array().cloned().unwrap_or_default() } else { arr };
    for (i, it) in arr.iter().take(8).enumerate() {
        let g = |k: &str| it.get(k).and_then(|x| x.as_str()).unwrap_or("");
        sb.push_str(&format!("{}. {}\n   {}\n   {}\n", i + 1, g(fields[0]), g(fields[1]), trunc(g(fields[2]), 500)));
    }
    if sb.is_empty() {
        sb.push_str("(no results)");
    }
    sb
}

/// Search the web with fallback across keyed providers (429-aware).
pub fn search(want: &str, query: &str) -> Result<String, WebError> {
    if query.trim().is_empty() {
        return Err(WebError::Msg("websearch: missing query".into()));
    }
    let want = want.trim().to_lowercase();
    let mut order = Vec::new();
    match want.as_str() {
        "" | "random" | "auto" => {
            for p in providers() {
                if !key_for(p).is_empty() && !cooling(p) {
                    order.push(*p);
                }
            }
        }
        p if providers().contains(&p) => order.push(p),
        _ => return Err(WebError::Msg(format!("websearch: unknown provider {want:?}"))),
    }
    let fixed = !matches!(want.as_str(), "" | "random" | "auto");
    if order.is_empty() {
        return Err(WebError::Msg(
            "websearch: no provider available (set TAVILY_API_KEY, EXA_API_KEY, FIRECRAWL_API_KEY, PARALLEL_API_KEY, or TINYFISH_API_KEY; pick with [websearch] provider)".into(),
        ));
    }
    let mut last = String::new();
    for p in order {
        if cooling(p) {
            continue;
        }
        match run_one(p, &key_for(p), query) {
            Ok(out) => return Ok(out),
            Err(WebError::Limited(ra)) => {
                backoff(p, &ra);
                last = "websearch: provider rate limited (429)".into();
            }
            Err(e) => {
                last = e.to_string();
                if fixed {
                    return Err(e);
                }
            }
        }
    }
    Err(WebError::Msg(if last.is_empty() {
        "websearch: every provider is cooling down after 429; retry shortly".into()
    } else {
        last
    }))
}

/// Fetch one HTTP(S) URL (cap 100KB); credential/Google targets are blocked,
/// and every redirect hop is re-checked (no silent cross-host hops).
pub fn fetch(raw_url: &str, format: &str, timeout: Duration) -> Result<(String, bool), WebError> {
    let raw_url = raw_url.trim();
    if raw_url.is_empty() {
        return Err(WebError::Msg("webfetch: missing url".into()));
    }
    let lower = raw_url.to_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(WebError::Msg("webfetch: only http(s) URLs are allowed".into()));
    }
    if crate::security::is_credential_path(raw_url) || crate::security::is_google_target(raw_url) {
        return Err(WebError::Msg("webfetch: blocked target".into()));
    }
    let policy = reqwest::redirect::Policy::custom(|attempt| {
        let url = attempt.url().as_str();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return attempt.stop();
        }
        if crate::security::is_credential_path(url) || crate::security::is_google_target(url) {
            return attempt.stop();
        }
        attempt.follow()
    });
    let res = reqwest::blocking::Client::builder()
        .user_agent("lumina/0.1.0")
        .timeout(timeout.min(Duration::from_secs(120)))
        .redirect(policy)
        .build()
        .map_err(|e| WebError::Msg(format!("webfetch: {e}")))?
        .get(raw_url)
        .header("Accept", "text/html,text/plain,application/json;q=0.9,*/*;q=0.5")
        .send()
        .map_err(|e| WebError::Msg(format!("webfetch: {e}")))?;
    if !res.status().is_success() {
        return Err(WebError::Msg(format!("webfetch: status {} for {raw_url}", res.status())));
    }
    let ct = res.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_lowercase();
    if ct.contains("image/") || ct.contains("video/") || ct.contains("audio/") || ct.contains("application/octet-stream") {
        return Err(WebError::Msg(format!("webfetch: binary content not supported ({ct})")));
    }
    const CAP: usize = 100 * 1024;
    let mut bytes = Vec::new();
    use std::io::Read;
    res.take(CAP as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| WebError::Msg(format!("webfetch: {e}")))?;
    let truncated = bytes.len() > CAP;
    if truncated {
        bytes.truncate(CAP);
    }
    let mut out = String::from_utf8_lossy(&bytes).into_owned();
    if format == "markdown" || format == "text" || format.is_empty() {
        if ct.contains("html") || looks_like_html(&out) {
            out = html_to_text(&out);
        }
    }
    if truncated {
        out.push_str("\n…[truncated at 100KB]");
    }
    Ok((out, truncated))
}

fn looks_like_html(s: &str) -> bool {
    let t = s.trim_start().to_lowercase();
    t.starts_with("<!doctype html") || t.starts_with("<html")
}

fn html_to_text(s: &str) -> String {
    // ponytail: regex-free tag strip (regex crate is heavier than needed here).
    let mut no_script = String::with_capacity(s.len());
    let lower = s.to_lowercase();
    let mut i = 0;
    let b = s.as_bytes();
    while i < b.len() {
        let rest = &lower[i..];
        let is_tag = |t: &str| rest.starts_with(t);
        if is_tag("<script") || is_tag("<style") || is_tag("<noscript") {
            let closer = if is_tag("<script") { "</script" } else if is_tag("<style") { "</style" } else { "</noscript" };
            match rest.find(closer) {
                Some(end) => {
                    i += end + closer.len();
                    continue;
                }
                None => break,
            }
        }
        no_script.push(b[i] as char);
        i += 1;
    }
    // Fold tags to spaces/newlines for block elements.
    let mut out = String::with_capacity(no_script.len());
    let mut k = 0;
    let nb = no_script.as_bytes();
    while k < nb.len() {
        if nb[k] == b'<' {
            if let Some(end) = no_script[k..].find('>') {
                let tag = no_script[k..k + end].to_lowercase();
                let block = ["p", "div", "br", "li", "ul", "ol", "tr", "table", "h1", "h2", "h3", "h4", "h5", "h6",
                    "section", "article", "header", "footer", "blockquote"]
                    .iter()
                    .any(|t| tag == format!("<{t}") || tag.starts_with(&format!("<{t} ")) || tag == format!("</{t}"));
                out.push(if block { '\n' } else { ' ' });
                k += end + 1;
                continue;
            }
        }
        out.push(nb[k] as char);
        k += 1;
    }
    out.split('\n')
        .map(|ln| ln.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|ln| !ln.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_google_and_creds() {
        assert!(fetch("https://generativelanguage.googleapis.com/x", "text", Duration::from_secs(5)).is_err());
        assert!(fetch("ftp://x/y", "text", Duration::from_secs(5)).is_err());
    }

    #[test]
    fn html_strips() {
        let t = html_to_text("<html><head><style>.a{}</style></head><body><h1>Hi</h1><p>there <b>x</b></p><script>evil()</script></body></html>");
        assert!(t.contains("Hi") && t.contains("there x"), "{t}");
        assert!(!t.contains("evil"));
    }

    #[test]
    fn no_provider_no_crash() {
        let e = search("tavily", "");
        assert!(e.is_err());
    }
}
