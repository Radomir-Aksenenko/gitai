//! Fast & token-efficient web search for LLM agents.
//!
//! Provides zero-config web search out of the box (via DuckDuckGo HTML), as well
//! as optional integrations with Tavily, Brave Search, and SearXNG.
//!
//! Output is aggressively distilled and sanitized:
//! - Tracking parameters (`utm_*`, `ref`, `gclid`, etc.) are stripped from URLs.
//! - HTML tags, entities, and advertisements are cleaned.
//! - Snippets are capped to ~150-200 characters of high-signal content.
//! - The total response for 3-5 results typically consumes only ~120-200 tokens.

use std::collections::HashSet;
use std::time::Duration;

use gitai_core::config::WebSearchConfig;
use gitai_core::error::{Error, Result};
use serde::{Deserialize, Serialize};

/// A single distilled web search result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

impl SearchResult {
    pub fn new(title: impl Into<String>, url: impl Into<String>, snippet: impl Into<String>) -> Self {
        let clean = clean_url(&url.into());
        Self {
            title: title.into().trim().to_string(),
            url: clean,
            snippet: snippet.into().trim().to_string(),
        }
    }
}

/// Web search engine supporting DuckDuckGo, Tavily, Brave, and SearXNG.
pub struct WebSearchEngine {
    client: reqwest::Client,
    cfg: WebSearchConfig,
}

impl WebSearchEngine {
    pub fn new(cfg: WebSearchConfig) -> Self {
        let timeout = Duration::from_secs(cfg.timeout_secs.max(1));
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36")
            .build()
            .unwrap_or_default();

        Self { client, cfg }
    }

    pub fn is_enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// Performs search for a single query.
    pub async fn search(&self, query: &str) -> Result<Vec<SearchResult>> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }

        let max_results = self.cfg.max_results.max(1).min(10);
        let provider = self.cfg.provider.to_lowercase();

        match provider.as_str() {
            "duckduckgo" | "ddg" => self.search_duckduckgo(q, max_results).await,
            "tavily" => self.search_tavily(q, max_results).await,
            "brave" => self.search_brave(q, max_results).await,
            "searxng" | "searx" => self.search_searxng(q, max_results).await,
            other => Err(Error::config(format!("unsupported web search provider `{other}`"))),
        }
    }

    /// Runs multiple search queries in parallel, deduplicates, and formats
    /// the results into a compact Markdown string tailored for LLM prompts.
    pub async fn search_all(&self, queries: &[String]) -> Result<String> {
        if queries.is_empty() {
            return Ok(String::new());
        }

        let mut output = String::new();
        let mut seen_urls = HashSet::new();

        for query in queries {
            let q = query.trim();
            if q.is_empty() {
                continue;
            }

            match self.search(q).await {
                Ok(results) => {
                    let unique_results: Vec<SearchResult> = results
                        .into_iter()
                        .filter(|r| seen_urls.insert(r.url.clone()))
                        .collect();

                    if !unique_results.is_empty() {
                        let formatted = format_results_compact(q, &unique_results, self.cfg.max_results, 220);
                        if !output.is_empty() {
                            output.push_str("\n\n");
                        }
                        output.push_str(&formatted);
                    }
                }
                Err(e) => {
                    tracing::warn!(query = %q, error = %e, "web search query failed");
                }
            }
        }

        if output.is_empty() {
            output = "No search results found.".to_string();
        }

        Ok(output)
    }

    // -----------------------------------------------------------------------
    // Provider Implementations
    // -----------------------------------------------------------------------

    /// DuckDuckGo HTML search (Zero-config, fast, no API key needed).
    async fn search_duckduckgo(&self, query: &str, max_results: usize) -> Result<Vec<SearchResult>> {
        let enc = url_encode(query);

        // 1. Try POST to html.duckduckgo.com with kl=wt-wt (worldwide)
        let url = "https://html.duckduckgo.com/html/";
        let body = format!("q={enc}&b=&kl=wt-wt");

        let res = self
            .client
            .post(url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
            .header("Accept-Language", "ru-RU,ru;q=0.9,en-US;q=0.8,en;q=0.7")
            .header("Referer", "https://html.duckduckgo.com/")
            .body(body)
            .send()
            .await;

        if let Ok(resp) = res {
            if let Ok(html) = resp.text().await {
                let results = parse_duckduckgo_html(&html, max_results);
                if !results.is_empty() {
                    return Ok(results);
                }
            }
        }

        // 2. Try POST to lite.duckduckgo.com
        let lite_url = "https://lite.duckduckgo.com/lite/";
        let lite_body = format!("q={enc}&kl=wt-wt");

        let lite_res = self
            .client
            .post(lite_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
            .header("Accept-Language", "ru-RU,ru;q=0.9,en-US;q=0.8,en;q=0.7")
            .header("Referer", "https://lite.duckduckgo.com/")
            .body(lite_body)
            .send()
            .await;

        if let Ok(resp) = lite_res {
            if let Ok(html) = resp.text().await {
                let results = parse_duckduckgo_html(&html, max_results);
                if !results.is_empty() {
                    return Ok(results);
                }
            }
        }

        // 3. Try GET to html.duckduckgo.com
        let get_url = format!("https://html.duckduckgo.com/html/?q={enc}&kl=wt-wt");
        let get_resp = self
            .client
            .get(&get_url)
            .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
            .header("Accept-Language", "ru-RU,ru;q=0.9,en-US;q=0.8,en;q=0.7")
            .header("Referer", "https://html.duckduckgo.com/")
            .send()
            .await
            .map_err(|e| Error::bad_output(format!("duckduckgo request failed: {e}")))?;

        let html = get_resp
            .text()
            .await
            .map_err(|e| Error::bad_output(format!("duckduckgo read body failed: {e}")))?;

        Ok(parse_duckduckgo_html(&html, max_results))
    }

    /// Tavily AI Search API (`https://api.tavily.com/search`).
    async fn search_tavily(&self, query: &str, max_results: usize) -> Result<Vec<SearchResult>> {
        if self.cfg.api_key.is_empty() {
            return Err(Error::config("tavily provider requires `api_key` in config"));
        }

        let url = if self.cfg.endpoint.is_empty() {
            "https://api.tavily.com/search"
        } else {
            &self.cfg.endpoint
        };

        let body = serde_json::json!({
            "query": query,
            "max_results": max_results,
            "search_depth": "basic",
            "include_answer": false,
            "include_raw_content": false,
        });

        let resp = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {}", self.cfg.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::bad_output(format!("tavily request failed: {e}")))?;

        let val: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::bad_output(format!("tavily json error: {e}")))?;

        parse_tavily_json(&val, max_results)
    }

    /// Brave Search API (`https://api.search.brave.com/res/v1/web/search`).
    async fn search_brave(&self, query: &str, max_results: usize) -> Result<Vec<SearchResult>> {
        if self.cfg.api_key.is_empty() {
            return Err(Error::config("brave provider requires `api_key` in config"));
        }

        let endpoint = if self.cfg.endpoint.is_empty() {
            "https://api.search.brave.com/res/v1/web/search"
        } else {
            &self.cfg.endpoint
        };

        let url = format!("{endpoint}?q={}&count={max_results}", url_encode(query));

        let resp = self
            .client
            .get(&url)
            .header("X-Subscription-Token", &self.cfg.api_key)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| Error::bad_output(format!("brave search request failed: {e}")))?;

        let val: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::bad_output(format!("brave json error: {e}")))?;

        parse_brave_json(&val, max_results)
    }

    /// SearXNG self-hosted meta-search (`{endpoint}/search?q={query}&format=json`).
    async fn search_searxng(&self, query: &str, max_results: usize) -> Result<Vec<SearchResult>> {
        if self.cfg.endpoint.is_empty() {
            return Err(Error::config("searxng provider requires `endpoint` in config"));
        }

        let base = self.cfg.endpoint.trim_end_matches('/');
        let url = format!("{base}/search?q={}&format=json", url_encode(query));

        let mut req = self.client.get(&url);
        if !self.cfg.api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", self.cfg.api_key));
        }

        let resp = req
            .send()
            .await
            .map_err(|e| Error::bad_output(format!("searxng request failed: {e}")))?;

        let val: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Error::bad_output(format!("searxng json error: {e}")))?;

        parse_searxng_json(&val, max_results)
    }
}

// ---------------------------------------------------------------------------
// Compact Formatting for LLM Token Efficiency
// ---------------------------------------------------------------------------

/// Formats a list of search results into a clean, compact Markdown block.
///
/// Designed to use the absolute minimum token count while retaining all essential facts.
pub fn format_results_compact(
    query: &str,
    results: &[SearchResult],
    max_count: usize,
    max_snippet_chars: usize,
) -> String {
    let mut out = format!("### Web Search: `{query}`\n");
    if results.is_empty() {
        out.push_str("No results found.\n");
        return out;
    }

    for (i, r) in results.iter().take(max_count).enumerate() {
        let snippet = truncate_at_word_boundary(&r.snippet, max_snippet_chars);
        out.push_str(&format!("{}. [{}]({})\n   {}\n", i + 1, r.title, r.url, snippet));
    }

    out
}

/// Truncates string at word boundary up to `max_chars`.
fn truncate_at_word_boundary(text: &str, max_chars: usize) -> String {
    let clean = text.replace('\n', " ").replace('\r', "");
    let trimmed = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.chars().count() <= max_chars {
        return trimmed;
    }

    let mut char_count = 0;
    let mut last_space_byte = 0;
    for (byte_idx, ch) in trimmed.char_indices() {
        if char_count >= max_chars {
            if last_space_byte > 0 {
                return format!("{}...", &trimmed[..last_space_byte]);
            } else {
                return format!("{}...", &trimmed[..byte_idx]);
            }
        }
        if ch.is_whitespace() {
            last_space_byte = byte_idx;
        }
        char_count += 1;
    }

    trimmed
}

/// Strips tracker parameters (`utm_*`, `ref`, `gclid`, `fbclid`, `session_id`, etc.) from URLs.
pub fn clean_url(raw_url: &str) -> String {
    let url = raw_url.trim();
    if url.is_empty() {
        return String::new();
    }

    // Decode DuckDuckGo redirect wrapper: /l/?kh=-1&uddg=https%3A%2F%2Fexample.com
    let target = if url.contains("duckduckgo.com/l/?") || url.starts_with("/l/?") {
        if let Some(pos) = url.find("uddg=") {
            let encoded = &url[pos + 5..];
            let end = encoded.find('&').unwrap_or(encoded.len());
            url_decode(&encoded[..end])
        } else {
            url.to_string()
        }
    } else {
        url.to_string()
    };

    let Some((base, query_str)) = target.split_once('?') else {
        return target;
    };

    let mut clean_params = Vec::new();
    for pair in query_str.split('&') {
        if pair.is_empty() {
            continue;
        }
        let key = pair.split('=').next().unwrap_or("").to_lowercase();
        if key.starts_with("utm_")
            || key == "ref"
            || key == "source"
            || key == "fbclid"
            || key == "gclid"
            || key == "gbraid"
            || key == "wbraid"
            || key == "msclkid"
            || key == "mc_eid"
            || key == "yclid"
            || key == "igshid"
            || key == "session_id"
        {
            continue;
        }
        clean_params.push(pair);
    }

    if clean_params.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", clean_params.join("&"))
    }
}

// ---------------------------------------------------------------------------
// HTML & JSON Parsers
// ---------------------------------------------------------------------------

/// Parses DuckDuckGo HTML response.
pub fn parse_duckduckgo_html(html: &str, max_results: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();

    // Strategy 1: HTML version with result__body / result__title / result__snippet
    let mut pos = 0;
    while let Some(res_start) = html[pos..].find("result__body") {
        let abs_start = pos + res_start;
        let next_result = html[abs_start + 12..]
            .find("result__body")
            .map(|offset| abs_start + 12 + offset)
            .unwrap_or(html.len().min(abs_start + 4000));

        let block = &html[abs_start..next_result];
        pos = abs_start + 12;

        // Title and Link: <a ... class="...result__a..." ... href="...">Title</a>
        let (title, url) = if let Some(a_start) = block.find("result__a") {
            let a_tag_start = block[..a_start].rfind("<a").unwrap_or(a_start);
            let after_a = &block[a_tag_start..];
            let href = extract_attribute(after_a, "href").unwrap_or_default();
            let title_text = extract_inner_text(after_a, "</a>").unwrap_or_default();
            (title_text, href)
        } else {
            continue;
        };

        // Snippet: contains result__snippet
        let snippet = if let Some(snip_start) = block.find("result__snippet") {
            let snip_tag_start = block[..snip_start].rfind('<').unwrap_or(snip_start);
            let after_snip = &block[snip_tag_start..];
            extract_inner_text(after_snip, "</a>")
                .or_else(|| extract_inner_text(after_snip, "</td"))
                .or_else(|| extract_inner_text(after_snip, "</div"))
                .unwrap_or_default()
        } else {
            String::new()
        };

        let clean_t = unescape_html(&strip_html_tags(&title));
        let clean_s = unescape_html(&strip_html_tags(&snippet));
        let clean_u = clean_url(&unescape_html(&url));

        if !clean_t.is_empty() && !clean_u.is_empty() && clean_u.starts_with("http") {
            results.push(SearchResult {
                title: clean_t,
                url: clean_u,
                snippet: clean_s,
            });
        }

        if results.len() >= max_results {
            break;
        }
    }

    // Strategy 2: Lite version with result-link and result-snippet
    if results.is_empty() {
        let mut pos = 0;
        while let Some(link_start) = html[pos..].find("result-link") {
            let abs_start = pos + link_start;
            let tag_start = html[..abs_start].rfind("<a").unwrap_or(abs_start);
            let after_a = &html[tag_start..];
            let href = extract_attribute(after_a, "href").unwrap_or_default();
            let title = extract_inner_text(after_a, "</a>").unwrap_or_default();

            let snippet = if let Some(snip_start) = html[abs_start..].find("result-snippet") {
                let snip_abs = abs_start + snip_start;
                let snip_after = &html[snip_abs..];
                extract_inner_text(snip_after, "</td").unwrap_or_default()
            } else {
                String::new()
            };

            pos = abs_start + 20;

            let clean_t = unescape_html(&strip_html_tags(&title));
            let clean_s = unescape_html(&strip_html_tags(&snippet));
            let clean_u = clean_url(&unescape_html(&href));

            if !clean_t.is_empty() && !clean_u.is_empty() && clean_u.starts_with("http") {
                results.push(SearchResult {
                    title: clean_t,
                    url: clean_u,
                    snippet: clean_s,
                });
            }

            if results.len() >= max_results {
                break;
            }
        }
    }

    // Strategy 3: Fallback parser on raw uddg links
    if results.is_empty() {
        results = parse_duckduckgo_fallback_links(html, max_results);
    }

    results
}

fn parse_duckduckgo_fallback_links(html: &str, max_results: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut pos = 0;

    while let Some(link_pos) = html[pos..].find("uddg=") {
        let abs_pos = pos + link_pos + 5;
        let end_pos = html[abs_pos..].find('"').unwrap_or(html[abs_pos..].find('&').unwrap_or(0));
        if end_pos > 0 {
            let raw_encoded = &html[abs_pos..abs_pos + end_pos];
            let decoded = url_decode(raw_encoded);
            if decoded.starts_with("http") && !results.iter().any(|r: &SearchResult| r.url == decoded) {
                results.push(SearchResult {
                    title: clean_url_title(&decoded),
                    url: decoded,
                    snippet: String::new(),
                });
            }
        }
        pos = abs_pos + 10;
        if results.len() >= max_results {
            break;
        }
    }

    results
}

fn clean_url_title(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string()
}

pub fn parse_tavily_json(val: &serde_json::Value, max_results: usize) -> Result<Vec<SearchResult>> {
    let mut results = Vec::new();
    if let Some(arr) = val.get("results").and_then(|v| v.as_array()) {
        for item in arr.iter().take(max_results) {
            let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let content = item.get("content").and_then(|v| v.as_str()).unwrap_or("");

            if !url.is_empty() {
                results.push(SearchResult::new(title, url, content));
            }
        }
    }
    Ok(results)
}

pub fn parse_brave_json(val: &serde_json::Value, max_results: usize) -> Result<Vec<SearchResult>> {
    let mut results = Vec::new();
    if let Some(arr) = val.pointer("/web/results").and_then(|v| v.as_array()) {
        for item in arr.iter().take(max_results) {
            let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let desc = item.get("description").and_then(|v| v.as_str()).unwrap_or("");

            if !url.is_empty() {
                results.push(SearchResult::new(title, url, desc));
            }
        }
    }
    Ok(results)
}

pub fn parse_searxng_json(val: &serde_json::Value, max_results: usize) -> Result<Vec<SearchResult>> {
    let mut results = Vec::new();
    if let Some(arr) = val.get("results").and_then(|v| v.as_array()) {
        for item in arr.iter().take(max_results) {
            let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let content = item.get("content").and_then(|v| v.as_str()).unwrap_or("");

            if !url.is_empty() {
                results.push(SearchResult::new(title, url, content));
            }
        }
    }
    Ok(results)
}

// ---------------------------------------------------------------------------
// HTML & URL Utilities
// ---------------------------------------------------------------------------

fn extract_attribute(tag_block: &str, attr: &str) -> Option<String> {
    let pattern = format!("{attr}=\"");
    if let Some(start) = tag_block.find(&pattern) {
        let after = &tag_block[start + pattern.len()..];
        if let Some(end) = after.find('"') {
            return Some(after[..end].to_string());
        }
    }
    None
}

fn extract_inner_text(block: &str, close_tag: &str) -> Option<String> {
    let start = block.find('>')? + 1;
    let after = &block[start..];
    let end = after.find(close_tag)?;
    Some(after[..end].to_string())
}

/// Strips `<tag ...>` from a string.
pub fn strip_html_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;

    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }

    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Unescapes common HTML entities.
pub fn unescape_html(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
        .replace("&#8211;", "-")
        .replace("&#8212;", "--")
        .replace("&#8216;", "'")
        .replace("&#8217;", "'")
        .replace("&#8220;", "\"")
        .replace("&#8221;", "\"")
        .replace("&hellip;", "...")
}

/// Simple percent-decoding for URLs.
pub fn url_decode(s: &str) -> String {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(val) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..=i + 2]).unwrap_or(""), 16) {
                out.push(val);
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }

    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// Simple percent-encoding for query strings.
pub fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_url_strips_utm_and_trackers() {
        let dirty = "https://docs.rs/reqwest/latest/reqwest/?utm_source=google&utm_medium=cpc&ref=twitter&good_param=123";
        let clean = clean_url(dirty);
        assert_eq!(clean, "https://docs.rs/reqwest/latest/reqwest/?good_param=123");
    }

    #[test]
    fn clean_url_decodes_duckduckgo_redirects() {
        let ddg_redirect = "/l/?kh=-1&uddg=https%3A%2F%2Fgithub.com%2Fseanmonstar%2Freqwest%3Futm_source%3Dddg";
        let clean = clean_url(ddg_redirect);
        assert_eq!(clean, "https://github.com/seanmonstar/reqwest");
    }

    #[test]
    fn unescape_html_and_strip_tags() {
        let raw = "<b>Reqwest</b> &amp; <i>Hyper</i> &quot;Client&quot; &#39;v0.12&#39;";
        let unescaped = unescape_html(raw);
        let stripped = strip_html_tags(&unescaped);
        assert_eq!(stripped, "Reqwest & Hyper \"Client\" 'v0.12'");
    }

    #[test]
    fn format_compact_is_tight_and_token_efficient() {
        let results = vec![
            SearchResult::new(
                "Reqwest Docs",
                "https://docs.rs/reqwest?utm_source=bad",
                "An ergonomic, batteries-included HTTP Client for Rust.",
            ),
            SearchResult::new(
                "Axum Web Framework",
                "https://github.com/tokio-rs/axum",
                "Ergonomic and modular web framework built with Tokio, Tower, and Hyper.",
            ),
        ];

        let formatted = format_results_compact("rust http client", &results, 5, 200);
        assert!(formatted.contains("### Web Search: `rust http client`"));
        assert!(formatted.contains("1. [Reqwest Docs](https://docs.rs/reqwest)"));
        assert!(formatted.contains("2. [Axum Web Framework](https://github.com/tokio-rs/axum)"));
        assert!(!formatted.contains("utm_source"));
    }

    #[test]
    fn parse_tavily_json_output() {
        let json = serde_json::json!({
            "results": [
                {
                    "title": "Rust Programming",
                    "url": "https://www.rust-lang.org/?utm_campaign=main",
                    "content": "A language empowering everyone to build reliable and efficient software."
                }
            ]
        });

        let results = parse_tavily_json(&json, 5).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust Programming");
        assert_eq!(results[0].url, "https://www.rust-lang.org/");
        assert!(results[0].snippet.contains("reliable and efficient"));
    }

    #[test]
    fn parse_brave_json_output() {
        let json = serde_json::json!({
            "web": {
                "results": [
                    {
                        "title": "Brave Search",
                        "url": "https://search.brave.com/",
                        "description": "Private search engine."
                    }
                ]
            }
        });

        let results = parse_brave_json(&json, 5).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Brave Search");
        assert_eq!(results[0].url, "https://search.brave.com/");
    }

    #[test]
    fn parse_duckduckgo_html_sample() {
        let html = r#"
        <div class="result results_links results_links_deep web-result ">
            <div class="result__body links_main links_deep">
                <h2 class="result__title">
                    <a class="result__a" href="/l/?kh=-1&uddg=https%3A%2F%2Fdocs.rs%2Freqwest"><b>Reqwest</b> in docs.rs</a>
                </h2>
                <a class="result__snippet" href="/l/?kh=-1&uddg=https%3A%2F%2Fdocs.rs%2Freqwest">An ergonomic, batteries-included HTTP Client for Rust.</a>
            </div>
        </div>
        "#;

        let results = parse_duckduckgo_html(html, 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Reqwest in docs.rs");
        assert_eq!(results[0].url, "https://docs.rs/reqwest");
        assert_eq!(results[0].snippet, "An ergonomic, batteries-included HTTP Client for Rust.");
    }
}
