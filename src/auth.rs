//! `bias login`, `logout` and `auth …`: OAuth sign-in in the browser (authorization code with
//! PKCE and a loopback redirect), or with a device code where no browser can open.

use crate::client::{self, CLIENT_ID, TokenGrant, random_token};
use crate::config::{self, Profile};
use crate::error::CliError;
use crate::output::{self, paint};
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

pub struct LoginOptions {
    pub profile: String,
    pub device: bool,
    pub read_only: bool,
    pub api_url: String,
}

pub fn dashboard_url(api_url: &str) -> String {
    if let Ok(url) = std::env::var("BIAS_DASHBOARD_URL") {
        return url.trim_end_matches('/').to_owned();
    }
    match reqwest::Url::parse(api_url) {
        Ok(mut url) => {
            if let Some(host) = url.host_str().and_then(|h| h.strip_prefix("api.")) {
                let host = format!("app.{host}");
                let _ = url.set_host(Some(&host));
            }
            url.set_path("");
            url.to_string().trim_end_matches('/').to_owned()
        }
        Err(_) => api_url.trim_end_matches('/').to_owned(),
    }
}

/// Over SSH, or on Linux without a display, a browser here can't reach the user.
fn can_open_browser() -> bool {
    if std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some() {
        return false;
    }
    if cfg!(target_os = "linux") {
        return std::env::var_os("DISPLAY").is_some()
            || std::env::var_os("WAYLAND_DISPLAY").is_some();
    }
    true
}

pub fn open_browser(url: &str) -> bool {
    if std::env::var_os("BIAS_NO_BROWSER").is_some() {
        return false;
    }
    let mut command = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else {
        std::process::Command::new("xdg-open")
    };
    command
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn device_name() -> String {
    let host = gethostname::gethostname().to_string_lossy().into_owned();
    host.strip_suffix(".local")
        .unwrap_or(&host)
        .chars()
        .take(64)
        .collect()
}

pub fn login(options: LoginOptions) -> Result<(), CliError> {
    let http = client::http();
    let scope = if options.read_only {
        "read"
    } else {
        "read write"
    };
    let grant = if options.device || !can_open_browser() {
        device_login(&http, &options.api_url, scope)?
    } else {
        browser_login(&http, &options.api_url, scope)?
    };

    let _lock = config::lock().map_err(CliError::Other)?;
    let mut config = config::load().map_err(CliError::Other)?;
    // Signing in again replaces the old sign-in, so revoke it rather than leave it active.
    if let Some(previous) = config.profiles.get(&options.profile)
        && let Ok(Some(tokens)) = config::load_tokens(&options.profile)
    {
        revoke(&http, &previous.api_url, &tokens.refresh_token);
        let _ = config::delete_tokens(&options.profile);
    }
    config::save_tokens(&options.profile, &grant.tokens()).map_err(CliError::Other)?;
    config.profiles.insert(
        options.profile.clone(),
        Profile {
            api_url: options.api_url.clone(),
            account: grant.account.clone(),
            scope: grant.scope.clone(),
            expires_at: grant.expires_at(),
        },
    );
    if config.default_profile.is_none() && options.profile != config::DEFAULT_PROFILE {
        config.default_profile = Some(options.profile.clone());
    }
    config::save(&config).map_err(CliError::Other)?;

    let color = output::stderr_color();
    eprintln!(
        "{} Signed in to {} ({}), {}{}.",
        paint(output::GREEN, "✓", color),
        paint(output::BOLD, &grant.account.name, color),
        grant.account.id,
        environment_label(grant.account.live, color),
        if grant.scope == "read" {
            ", read-only"
        } else {
            ""
        },
    );
    if options.profile != config::DEFAULT_PROFILE {
        eprintln!("  Saved as profile {}.", options.profile);
    }
    Ok(())
}

fn environment_label(live: bool, color: bool) -> String {
    if live {
        paint(output::YELLOW, "live", color)
    } else {
        "sandbox".into()
    }
}

fn browser_login(
    http: &reqwest::blocking::Client,
    api_url: &str,
    scope: &str,
) -> Result<TokenGrant, CliError> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| CliError::Other(format!("couldn't listen for the sign-in redirect: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| CliError::Other(e.to_string()))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let verifier = random_token(48);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let state = random_token(16);

    let mut url = reqwest::Url::parse(&format!("{}/oauth/authorize", dashboard_url(api_url)))
        .map_err(|e| CliError::Other(format!("bad dashboard URL: {e}")))?;
    url.query_pairs_mut()
        .append_pair("client_id", CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("scope", scope)
        .append_pair("state", &state)
        .append_pair("device_name", &device_name());

    let opened = open_browser(url.as_str());
    eprintln!(
        "{} the dashboard to sign in. If it didn't open, visit:\n\n  {url}\n",
        if opened { "Opened" } else { "Open" }
    );
    eprintln!("Waiting for the sign-in… (Ctrl-C to cancel, or run `bias login --device` instead)");

    let (code, mut stream) = wait_for_redirect(&listener, &state)?;
    // Answer the browser only once the code is exchanged, so its page reports what really happened.
    let result = code.and_then(|code| {
        let response = http
            .post(format!("{}/v1/oauth/token", api_url.trim_end_matches('/')))
            .timeout(Duration::from_secs(30))
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", CLIENT_ID),
                ("code", code.as_str()),
                ("code_verifier", verifier.as_str()),
                ("redirect_uri", redirect_uri.as_str()),
            ])
            .send()
            .map_err(|e| CliError::Network(format!("couldn't reach {api_url}: {e}")))?;
        token_response(response)
    });

    let mut outcome = reqwest::Url::parse(&format!("{}/oauth/complete", dashboard_url(api_url)))
        .map_err(|e| CliError::Other(format!("bad dashboard URL: {e}")))?;
    match &result {
        Ok(grant) => {
            outcome
                .query_pairs_mut()
                .append_pair("status", "success")
                .append_pair("account", &grant.account.name);
        }
        Err(CliError::Auth(message)) if message.contains("denied") => {
            outcome.query_pairs_mut().append_pair("status", "denied");
        }
        Err(_) => {
            outcome.query_pairs_mut().append_pair("status", "error");
        }
    }
    let _ = write!(
        stream,
        "HTTP/1.1 302 Found\r\nLocation: {outcome}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    result
}

type Redirect = (Result<String, CliError>, TcpStream);

fn wait_for_redirect(listener: &TcpListener, state: &str) -> Result<Redirect, CliError> {
    listener
        .set_nonblocking(true)
        .map_err(|e| CliError::Other(e.to_string()))?;
    let deadline = Instant::now() + LOGIN_TIMEOUT;
    while Instant::now() < deadline {
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(CliError::Other(format!("sign-in redirect failed: {e}"))),
        };
        if let Some(redirect) = read_redirect(stream, state) {
            return Ok(redirect);
        }
    }
    Err(CliError::Auth(
        "the sign-in timed out. Run `bias login` to try again.".into(),
    ))
}

/// The code from the dashboard's redirect, and the connection to answer it on. `None` for other
/// requests, such as a browser asking for a favicon.
fn read_redirect(mut stream: TcpStream, state: &str) -> Option<Redirect> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).ok()?;
    let target = line.split_whitespace().nth(1)?;
    let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    if url.path() != "/callback" {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return None;
    }
    let param = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    let result = if param("state").as_deref() != Some(state) {
        Err(CliError::Auth(
            "the sign-in redirect didn't match this login. Run `bias login` again.".into(),
        ))
    } else if let Some(error) = param("error") {
        Err(CliError::Auth(match error.as_str() {
            "access_denied" => "the sign-in was denied in the dashboard.".to_owned(),
            _ => param("error_description").unwrap_or(error),
        }))
    } else {
        param("code").ok_or_else(|| CliError::Auth("the sign-in redirect had no code.".into()))
    };
    Some((result, stream))
}

fn device_login(
    http: &reqwest::blocking::Client,
    api_url: &str,
    scope: &str,
) -> Result<TokenGrant, CliError> {
    let base = api_url.trim_end_matches('/');
    let response = http
        .post(format!("{base}/v1/oauth/device/code"))
        .timeout(Duration::from_secs(30))
        .form(&[
            ("client_id", CLIENT_ID),
            ("scope", scope),
            ("device_name", &device_name()),
        ])
        .send()
        .map_err(|e| CliError::Network(format!("couldn't reach {api_url}: {e}")))?;
    let status = response.status();
    let body: Value = response.json().unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(oauth_error(&body));
    }
    let text = |name: &str| {
        body.get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let device_code = text("device_code");
    let user_code = text("user_code");
    let verification_uri = text("verification_uri");
    let complete = text("verification_uri_complete");
    let mut interval = body.get("interval").and_then(Value::as_u64).unwrap_or(5);
    let expires_in = body
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(900);

    let color = output::stderr_color();
    eprintln!(
        "To sign in, open {} and enter this code:\n\n  {}\n",
        paint(output::BOLD, &verification_uri, color),
        paint(output::BOLD, &user_code, color),
    );
    if can_open_browser() && !complete.is_empty() && open_browser(&complete) {
        eprintln!("Opened the dashboard in your browser.");
    }
    eprintln!("Waiting for approval… (Ctrl-C to cancel)");

    let deadline = Instant::now() + Duration::from_secs(expires_in);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(interval));
        let response = http
            .post(format!("{base}/v1/oauth/token"))
            .timeout(Duration::from_secs(30))
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", CLIENT_ID),
                ("device_code", device_code.as_str()),
            ])
            .send();
        let Ok(response) = response else { continue };
        if response.status().is_success() {
            return token_response(response);
        }
        let body: Value = response.json().unwrap_or(Value::Null);
        match body.get("error").and_then(Value::as_str) {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += 5,
            _ => return Err(oauth_error(&body)),
        }
    }
    Err(CliError::Auth(
        "the code expired before it was approved. Run `bias login` again.".into(),
    ))
}

fn token_response(response: reqwest::blocking::Response) -> Result<TokenGrant, CliError> {
    let status = response.status();
    let body: Value = response.json().unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(oauth_error(&body));
    }
    TokenGrant::from_json(&body)
}

fn oauth_error(body: &Value) -> CliError {
    let message = match body.get("error").and_then(Value::as_str) {
        Some("access_denied") => "the sign-in was denied in the dashboard.".to_owned(),
        Some("expired_token") => {
            "the code expired before it was approved. Run `bias login` again.".to_owned()
        }
        _ => body
            .get("error_description")
            .and_then(Value::as_str)
            .unwrap_or("the sign-in failed")
            .to_owned(),
    };
    CliError::Auth(message)
}

fn revoke(http: &reqwest::blocking::Client, api_url: &str, token: &str) {
    let _ = http
        .post(format!("{}/v1/oauth/revoke", api_url.trim_end_matches('/')))
        .timeout(Duration::from_secs(10))
        .form(&[("client_id", CLIENT_ID), ("token", token)])
        .send();
}

pub fn logout(profile: Option<&str>, all: bool) -> Result<(), CliError> {
    let http = client::http();
    let _lock = config::lock().map_err(CliError::Other)?;
    let mut config = config::load().map_err(CliError::Other)?;
    let names: Vec<String> = if all {
        config.profiles.keys().cloned().collect()
    } else {
        vec![profile.unwrap_or(config.default_profile_name()).to_owned()]
    };
    if names.is_empty() || (!all && !config.profiles.contains_key(&names[0])) {
        eprintln!(
            "Not signed in{}.",
            if all { "" } else { " with that profile" }
        );
        return Ok(());
    }
    for name in names {
        let Some(profile) = config.profiles.remove(&name) else {
            continue;
        };
        if let Ok(Some(tokens)) = config::load_tokens(&name) {
            revoke(&http, &profile.api_url, &tokens.refresh_token);
        }
        config::delete_tokens(&name).map_err(CliError::Other)?;
        if config.default_profile.as_deref() == Some(name.as_str()) {
            config.default_profile = None;
        }
        eprintln!("Signed out of {} ({}).", profile.account.name, name);
    }
    config::save(&config).map_err(CliError::Other)
}

pub fn status() -> Result<(), CliError> {
    let config = config::load().map_err(CliError::Other)?;
    if config.profiles.is_empty() {
        println!("Not signed in. Run `bias login`.");
        return Ok(());
    }
    let color = output::stdout_color();
    let default = config.default_profile_name();
    for (name, profile) in &config.profiles {
        let marker = if name == default { "*" } else { " " };
        println!(
            "{marker} {}  {} ({}), {}{}",
            paint(output::BOLD, name, color),
            profile.account.name,
            profile.account.id,
            environment_label(profile.account.live, color),
            if profile.scope == "read" {
                ", read-only"
            } else {
                ""
            },
        );
        if profile.api_url != crate::manifest::manifest().base_url {
            println!("    {}", paint(output::DIM, &profile.api_url, color));
        }
    }
    Ok(())
}

pub fn switch(name: &str) -> Result<(), CliError> {
    let _lock = config::lock().map_err(CliError::Other)?;
    let mut config = config::load().map_err(CliError::Other)?;
    let Some(profile) = config.profiles.get(name) else {
        let names: Vec<String> = config.profiles.keys().cloned().collect();
        return Err(CliError::Usage(if names.is_empty() {
            "no profiles yet. Run `bias login`.".into()
        } else {
            format!("no profile {name}. Profiles: {}", names.join(", "))
        }));
    };
    eprintln!("Commands now use {} ({}).", profile.account.name, name);
    config.default_profile = Some(name.to_owned());
    config::save(&config).map_err(CliError::Other)
}

/// A profile's signed-in state, for making requests.
pub fn load_profile(name: &str) -> Result<(Profile, config::Tokens), CliError> {
    let config = config::load().map_err(CliError::Other)?;
    let Some(profile) = config.profiles.get(name).cloned() else {
        return Err(CliError::SignedOut {
            message: client::signed_out_message(name),
            profile: name.to_owned(),
        });
    };
    let tokens = config::load_tokens(name)
        .map_err(CliError::Other)?
        .ok_or_else(|| CliError::SignedOut {
            message: client::signed_out_message(name),
            profile: name.to_owned(),
        })?;
    Ok((profile, tokens))
}
