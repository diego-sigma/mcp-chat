//! OAuth 2.0 *client_credentials* grant — generic, endpoint-agnostic.
//!
//! Holds the latest access token in memory with a 60-second skew window;
//! re-fetches transparently when the cache is empty or about to expire.

use anyhow::{Context, Result};
use base64::Engine;
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

struct CachedToken {
    token: String,
    expires_at: Instant,
}

#[derive(Clone)]
pub struct TokenManager {
    client_id: String,
    client_secret: String,
    token_endpoint: String,
    http: reqwest::Client,
    cache: Arc<Mutex<Option<CachedToken>>>,
}

impl TokenManager {
    /// `token_endpoint` is the full URL of the OAuth token endpoint
    /// (e.g. `https://auth.example.com/oauth/token`). Credentials are sent
    /// via HTTP Basic auth as per RFC 6749 §2.3.1.
    pub fn new(client_id: String, client_secret: String, token_endpoint: String) -> Self {
        Self {
            client_id,
            client_secret,
            token_endpoint,
            http: reqwest::Client::new(),
            cache: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn get_valid_token(&self) -> Result<String> {
        let mut cache = self.cache.lock().await;
        if let Some(t) = cache.as_ref()
            && Instant::now() + Duration::from_secs(60) < t.expires_at
        {
            return Ok(t.token.clone());
        }
        let fresh = self.fetch_token().await?;
        let token = fresh.access_token.clone();
        *cache = Some(CachedToken {
            token: fresh.access_token,
            expires_at: Instant::now() + Duration::from_secs(fresh.expires_in),
        });
        Ok(token)
    }

    async fn fetch_token(&self) -> Result<TokenResponse> {
        let creds = format!("{}:{}", self.client_id, self.client_secret);
        let basic = base64::engine::general_purpose::STANDARD.encode(creds);
        let resp = self
            .http
            .post(&self.token_endpoint)
            .header("Authorization", format!("Basic {basic}"))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body("grant_type=client_credentials")
            .send()
            .await
            .with_context(|| format!("POST {}", self.token_endpoint))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("auth failed: {status} {body}");
        }
        resp.json::<TokenResponse>()
            .await
            .context("parse token response")
    }
}
