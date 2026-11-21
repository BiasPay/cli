//! Sends requests with the signed-in profile's token or an API key: refreshes tokens as they
//! expire, retries what's safe to retry, and turns API errors into readable ones.

use crate::config::{self, Profile, Tokens};
use crate::error::CliError;
use reqwest::blocking::{Client as Http, RequestBuilder, Response};
use reqwest::header::HeaderMap;
use serde_json::Value;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const CLIENT_ID: &str = "bias-cli";
const USER_AGENT: &str = concat!("bias-cli/", env!("CARGO_PKG_VERSION"));
const MAX_RETRIES: u32 = 2;
const BASE_RETRY_DELAY: Duration = Duration::from_millis(500);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(15);
/// Refresh a token this long before it expires, so it doesn't lapse mid-request.
const REFRESH_MARGIN_MS: u64 = 60_000;

pub enum Auth {
    ApiKey(String),
    Profile {
        name: String,
        profile: Profile,
        tokens: Tokens,
    },
}

pub struct Client {
    pub http: Http,
    pub base_url: String,
    pub auth: Auth,
}

pub enum Payload {
    None,
    Json(Value),
    Multipart(Vec<(String, MultipartValue)>),
}

pub enum MultipartValue {
    Text(String),
    File(String),
}

pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a [(String, String)],
    pub payload: &'a Payload,
    pub idempotency_key: Option<&'a str>,
    /// Keep the connection open with no overall timeout, for streams.
    pub stream: bool,
}

pub fn http() -> Http {
    // Errs only when a provider is already installed, which is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();
    Http::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(10))
        .timeout(None)
        .build()
        .expect("HTTP client builds")
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Client {
    pub fn is_live(&self) -> bool {
        match &self.auth {
            Auth::ApiKey(key) => key.starts_with("sk_live_"),
            Auth::Profile { profile, .. } => profile.account.live,
        }
    }

    pub fn account_label(&self) -> String {
        match &self.auth {
            Auth::ApiKey(_) => "the API key's account".into(),
            Auth::Profile { profile, .. } => {
                format!("{} ({})", profile.account.name, profile.account.id)
            }
        }
    }

    fn bearer(&self) -> &str {
        match &self.auth {
            Auth::ApiKey(key) => key,
            Auth::Profile { tokens, .. } => &tokens.access_token,
        }
    }

    pub fn send(&mut self, request: &Request) -> Result<Response, CliError> {
        if let Auth::Profile { profile, .. } = &self.auth
            && profile.expires_at <= now_ms() + REFRESH_MARGIN_MS
        {
            self.refresh(false)?;
        }
        let mut refreshed = false;
        let mut attempt = 0;
        loop {
            let response = self.build(request)?.send();
            let retryable_write = request.method == "GET" || request.idempotency_key.is_some();
            match response {
                Ok(response) => {
                    let status = response.status().as_u16();
                    if status == 401 && !refreshed && matches!(self.auth, Auth::Profile { .. }) {
                        // The token may have been revoked or rotated by another command; a fresh
                        // one settles which.
                        refreshed = true;
                        self.refresh(true)?;
                        continue;
                    }
                    let retry = attempt < MAX_RETRIES
                        && (status == 429 || (status >= 500 && retryable_write));
                    if !retry {
                        return Ok(response);
                    }
                    let delay = retry_after(response.headers()).unwrap_or_else(|| backoff(attempt));
                    std::thread::sleep(delay.min(MAX_RETRY_DELAY));
                }
                Err(e)
                    if attempt < MAX_RETRIES
                        && retryable_write
                        && (e.is_connect() || e.is_timeout()) =>
                {
                    std::thread::sleep(backoff(attempt));
                }
                Err(e) => return Err(CliError::Network(network_message(&self.base_url, &e))),
            }
            attempt += 1;
        }
    }

    fn build(&self, request: &Request) -> Result<RequestBuilder, CliError> {
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), request.path);
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|_| CliError::Usage(format!("unknown HTTP method {}", request.method)))?;
        let mut builder = self
            .http
            .request(method, url)
            .bearer_auth(self.bearer())
            .query(request.query);
        if !request.stream {
            builder = builder.timeout(Duration::from_secs(80));
        }
        if let Some(key) = request.idempotency_key {
            builder = builder.header("Idempotency-Key", key);
        }
        builder = match request.payload {
            Payload::None => builder,
            Payload::Json(value) => builder.json(value),
            Payload::Multipart(parts) => {
                let mut form = reqwest::blocking::multipart::Form::new();
                for (name, value) in parts {
                    form = match value {
                        MultipartValue::Text(text) => form.text(name.clone(), text.clone()),
                        MultipartValue::File(path) => form
                            .file(name.clone(), path)
                            .map_err(|e| CliError::Other(format!("couldn't read {path}: {e}")))?,
                    };
                }
                builder.multipart(form)
            }
        };
        Ok(builder)
    }

    /// Swaps the refresh token for new tokens, unless another command already did (`force`
    /// refreshes anyway, after the API refused the token it has).
    fn refresh(&mut self, force: bool) -> Result<(), CliError> {
        let Auth::Profile {
            name,
            profile,
            tokens,
        } = &mut self.auth
        else {
            return Ok(());
        };
        let _lock = config::lock().map_err(CliError::Other)?;
        let mut config = config::load().map_err(CliError::Other)?;
        let Some(stored) = config.profiles.get(name).cloned() else {
            return Err(signed_out(name));
        };
        let current = config::load_tokens(name)
            .map_err(CliError::Other)?
            .ok_or_else(|| signed_out(name))?;
        let rotated_elsewhere = current.access_token != tokens.access_token;
        if rotated_elsewhere && stored.expires_at > now_ms() + REFRESH_MARGIN_MS {
            *profile = stored;
            *tokens = current;
            return Ok(());
        }
        if !force && stored.expires_at > now_ms() + REFRESH_MARGIN_MS {
            return Ok(());
        }

        let response = self
            .http
            .post(format!(
                "{}/v1/oauth/token",
                stored.api_url.trim_end_matches('/')
            ))
            .timeout(Duration::from_secs(30))
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", CLIENT_ID),
                ("refresh_token", current.refresh_token.as_str()),
            ])
            .send()
            .map_err(|e| CliError::Network(network_message(&stored.api_url, &e)))?;
        let status = response.status();
        let body: Value = response.json().unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(CliError::SignedOut {
                message: match body.get("error_description").and_then(Value::as_str) {
                    Some(d) if d.contains("revoked") || d.contains("already used") => {
                        "This sign-in was signed out from the dashboard or another session.".into()
                    }
                    _ => "Your sign-in has expired.".into(),
                },
                profile: name.clone(),
            });
        }
        let grant = TokenGrant::from_json(&body)?;
        let new_tokens = grant.tokens();
        config::save_tokens(name, &new_tokens).map_err(CliError::Other)?;
        let mut updated = stored;
        updated.expires_at = grant.expires_at();
        updated.scope = grant.scope.clone();
        updated.account = grant.account.clone();
        config.profiles.insert(name.clone(), updated.clone());
        config::save(&config).map_err(CliError::Other)?;
        *profile = updated;
        *tokens = new_tokens;
        Ok(())
    }
}

/// The token endpoint's response.
pub struct TokenGrant {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
    pub scope: String,
    pub account: config::Account,
}

impl TokenGrant {
    pub fn from_json(body: &Value) -> Result<Self, CliError> {
        let field = |name: &str| {
            body.get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| CliError::Other(format!("the token response has no {name}")))
        };
        let account = body
            .get("account")
            .ok_or_else(|| CliError::Other("the token response has no account".into()))?;
        Ok(TokenGrant {
            access_token: field("access_token")?,
            refresh_token: field("refresh_token")?,
            expires_in: body
                .get("expires_in")
                .and_then(Value::as_u64)
                .unwrap_or(3600),
            scope: field("scope")?,
            account: serde_json::from_value(account.clone())
                .map_err(|e| CliError::Other(format!("the token response's account: {e}")))?,
        })
    }

    pub fn tokens(&self) -> Tokens {
        Tokens {
            access_token: self.access_token.clone(),
            refresh_token: self.refresh_token.clone(),
        }
    }

    pub fn expires_at(&self) -> u64 {
        now_ms() + self.expires_in * 1000
    }
}

pub fn profile_flag(name: &str) -> String {
    if name == config::DEFAULT_PROFILE {
        String::new()
    } else {
        format!(" --profile {name}")
    }
}

fn signed_out(name: &str) -> CliError {
    CliError::SignedOut {
        message: signed_out_message(name),
        profile: name.to_owned(),
    }
}

pub fn signed_out_message(profile: &str) -> String {
    if profile == config::DEFAULT_PROFILE {
        "You're not signed in.".into()
    } else {
        format!("You're not signed in as {profile}.")
    }
}

fn backoff(attempt: u32) -> Duration {
    let ceiling = BASE_RETRY_DELAY * 2u32.pow(attempt.min(8));
    let mut byte = [0u8; 1];
    let _ = getrandom::fill(&mut byte);
    ceiling.mul_f64(f64::from(byte[0]) / 255.0)
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get("retry-after")?.to_str().ok()?;
    value
        .trim()
        .parse::<f64>()
        .ok()
        .and_then(|s| Duration::try_from_secs_f64(s.max(0.0)).ok())
}

fn network_message(base_url: &str, error: &reqwest::Error) -> String {
    let mut message = format!("couldn't reach {base_url}: {error}");
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        message.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    message
}

pub fn random_token(bytes: usize) -> String {
    use base64::Engine;
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("system randomness");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}
