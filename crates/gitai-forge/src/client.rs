//! Thin JSON client shared by the forge adapters. Nothing clever: it exists so
//! error messages carry the forge name, the path and the response body, which
//! is what you actually need when a self-hosted instance answers 404 because a
//! token lacks one scope.

use std::time::Duration;

use gitai_core::error::{Error, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub struct ApiClient {
    forge: String,
    base_url: String,
    auth_header: Option<(String, String)>,
    extra_headers: Vec<(String, String)>,
    client: reqwest::Client,
}

impl ApiClient {
    pub fn new(
        forge: impl Into<String>,
        base_url: impl Into<String>,
        auth_header: Option<(String, String)>,
        extra_headers: Vec<(String, String)>,
        timeout_secs: u64,
    ) -> Result<Self> {
        let forge = forge.into();
        let client = reqwest::Client::builder()
            .user_agent(gitai_core::USER_AGENT)
            .timeout(Duration::from_secs(timeout_secs.max(1)))
            .build()
            .map_err(|e| Error::forge(&forge, e))?;
        Ok(Self {
            forge,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            auth_header,
            extra_headers,
            client,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{}", self.base_url, path);
        let mut b = self.client.request(method, url);
        if let Some((name, value)) = &self.auth_header {
            b = b.header(name, value);
        }
        for (k, v) in &self.extra_headers {
            b = b.header(k, v);
        }
        b
    }

    async fn send<T: DeserializeOwned>(
        &self,
        builder: reqwest::RequestBuilder,
        what: &str,
    ) -> Result<T> {
        let body = self.send_raw(builder, what).await?;
        // A 204 has no body; deserialize null so callers asking for `()` work.
        let text = if body.trim().is_empty() {
            "null"
        } else {
            &body
        };
        serde_json::from_str(text)
            .map_err(|e| Error::forge(&self.forge, format!("{what}: bad JSON: {e}; body: {body}")))
    }

    async fn send_raw(&self, builder: reqwest::RequestBuilder, what: &str) -> Result<String> {
        let resp = builder
            .send()
            .await
            .map_err(|e| Error::forge(&self.forge, format!("{what}: {e}")))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Error::forge(
                &self.forge,
                format!("{what}: http {status}: {}", truncate(&body, 400)),
            ));
        }
        Ok(body)
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str, what: &str) -> Result<T> {
        self.send(self.request(reqwest::Method::GET, path), what)
            .await
    }

    pub async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        what: &str,
    ) -> Result<T> {
        self.send(self.request(reqwest::Method::POST, path).json(body), what)
            .await
    }

    pub async fn post_ignore<B: Serialize>(&self, path: &str, body: &B, what: &str) -> Result<()> {
        self.send_raw(self.request(reqwest::Method::POST, path).json(body), what)
            .await?;
        Ok(())
    }

    pub async fn put<B: Serialize>(&self, path: &str, body: &B, what: &str) -> Result<()> {
        self.send_raw(self.request(reqwest::Method::PUT, path).json(body), what)
            .await?;
        Ok(())
    }

    pub async fn delete(&self, path: &str, what: &str) -> Result<()> {
        self.send_raw(self.request(reqwest::Method::DELETE, path), what)
            .await?;
        Ok(())
    }

    /// Like [`delete`], but a 404 counts as success. Used for cleanup, where
    /// "it is already gone" is the outcome we wanted.
    pub async fn delete_idempotent(&self, path: &str, what: &str) -> Result<()> {
        match self.delete(path, what).await {
            Err(Error::Forge { msg, .. }) if msg.contains("http 404") => Ok(()),
            other => other,
        }
    }
}

/// Percent-encodes a ref name segment by segment, leaving the separators in
/// place. `gitai/issue-7/r0-a0` has to stay a path in the git refs API, so it
/// cannot go through [`esc`] whole.
pub fn esc_ref(name: &str) -> String {
    name.split('/').map(esc).collect::<Vec<_>>().join("/")
}

/// Percent-encodes one path segment. Repository and label names reach the URL
/// straight from user input.
pub fn esc(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while end < s.len() && !s.is_char_boundary(end) {
        end += 1;
    }
    format!("{}...", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_segments_are_escaped() {
        assert_eq!(esc("plain-name_1.0"), "plain-name_1.0");
        assert_eq!(esc("needs space"), "needs%20space");
        assert_eq!(esc("a/b"), "a%2Fb");
        assert_eq!(esc("баг"), "%D0%B1%D0%B0%D0%B3");
    }

    #[test]
    fn ref_names_keep_their_separators() {
        assert_eq!(esc_ref("gitai/issue-7/r0-a0"), "gitai/issue-7/r0-a0");
        assert_eq!(esc_ref("feature/my branch"), "feature/my%20branch");
        assert_eq!(
            esc("gitai/issue-7"),
            "gitai%2Fissue-7",
            "esc still encodes slashes"
        );
    }

    #[test]
    fn base_url_loses_its_trailing_slash() {
        let c = ApiClient::new("t", "https://example.com/api/v1/", None, vec![], 5).unwrap();
        assert_eq!(c.base_url(), "https://example.com/api/v1");
    }
}
