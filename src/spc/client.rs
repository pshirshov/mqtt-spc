use reqwest::Client;
use tracing::{debug, info, warn};

use crate::config::Credentials;

pub struct SpcClient {
    http: Client,
    base_url: String,
    user_id: String,
    password: String,
    session: Option<String>,
}

impl SpcClient {
    pub fn new(url: &str, creds: &Credentials) -> Self {
        let http = Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .expect("failed to build HTTP client");

        Self {
            http,
            base_url: url.trim_end_matches('/').to_string(),
            user_id: creds.login.clone(),
            password: creds.password.clone(),
            session: None,
        }
    }

    pub async fn login(&mut self) -> Result<(), String> {
        let url = format!(
            "{}/login.htm?action=login&language=0",
            self.base_url
        );
        let body = format!("userid={}&password={}", self.user_id, self.password);

        debug!("Logging in to {url}");

        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|e| format!("login request failed: {e}"))?;

        let html = resp
            .text()
            .await
            .map_err(|e| format!("login response read failed: {e}"))?;

        // Extract session token from HTML: session=0x<HEX>
        let token = extract_session_token(&html)
            .ok_or_else(|| format!("no session token found in login response"))?;

        info!("SPC login OK, session={token}");
        self.session = Some(token);
        Ok(())
    }

    pub async fn fetch_page(&mut self, page: &str) -> Result<String, String> {
        if self.session.is_none() {
            self.login().await?;
        }

        let session = self.session.as_ref().unwrap();
        let url = format!(
            "{}/secure.htm?session={session}&page={page}",
            self.base_url
        );

        debug!("Fetching {page}");

        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("fetch {page} failed: {e}"))?;

        let status = resp.status();
        let html = resp
            .text()
            .await
            .map_err(|e| format!("read {page} response failed: {e}"))?;

        // Detect session expiry — login page typically lacks "session=" in links
        if status.is_client_error() || !html.contains("session=") {
            warn!("Session expired, re-logging in");
            self.session = None;
            self.login().await?;
            // Retry once
            let session = self.session.as_ref().unwrap();
            let url = format!(
                "{}/secure.htm?session={session}&page={page}",
                self.base_url
            );
            let resp = self
                .http
                .get(&url)
                .send()
                .await
                .map_err(|e| format!("retry fetch {page} failed: {e}"))?;
            return resp
                .text()
                .await
                .map_err(|e| format!("retry read {page} failed: {e}"));
        }

        Ok(html)
    }

    pub async fn post_command_to_page(&mut self, page: &str, button: &str) -> Result<(), String> {
        if self.session.is_none() {
            self.login().await?;
        }

        let session = self.session.as_ref().unwrap();
        let url = format!(
            "{}/secure.htm?session={session}&page={page}&action=update",
            self.base_url
        );

        info!("Posting command: {button}");

        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!("{button}="))
            .send()
            .await
            .map_err(|e| format!("command {button} failed: {e}"))?;

        let status = resp.status();
        if !status.is_success() && !status.is_redirection() {
            return Err(format!("command {button} returned HTTP {status}"));
        }

        Ok(())
    }
}

fn extract_session_token(html: &str) -> Option<String> {
    // Look for session=0x<HEX> or session=<TOKEN> in the HTML
    let marker = "session=";
    let pos = html.find(marker)?;
    let start = pos + marker.len();
    let rest = &html[start..];
    // Token ends at '&', '"', '\'', ' ', or '<'
    let end = rest
        .find(|c: char| c == '&' || c == '"' || c == '\'' || c == ' ' || c == '<')
        .unwrap_or(rest.len());
    let token = &rest[..end];
    if token.is_empty() {
        return None;
    }
    Some(token.to_string())
}
