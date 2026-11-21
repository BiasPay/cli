//! Profiles in `config.json`, and their tokens in `credentials.json`, both readable only by the
//! user (as the Stripe CLI keeps its keys).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

pub const DEFAULT_PROFILE: &str = "default";

#[derive(Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Profile {
    pub api_url: String,
    pub account: Account,
    pub scope: String,
    /// When the access token expires, in Unix milliseconds.
    pub expires_at: u64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub live: bool,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
}

impl Config {
    pub fn default_profile_name(&self) -> &str {
        self.default_profile.as_deref().unwrap_or(DEFAULT_PROFILE)
    }
}

pub fn dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("BIAS_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    if cfg!(windows)
        && let Some(appdata) = std::env::var_os("APPDATA")
    {
        return PathBuf::from(appdata).join("bias");
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(xdg).join("bias");
    }
    home().join(".config").join("bias")
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn load() -> Result<Config, String> {
    let path = dir().join("config.json");
    match fs::read_to_string(&path) {
        Ok(text) => {
            serde_json::from_str(&text).map_err(|e| format!("{} is malformed: {e}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(format!("couldn't read {}: {e}", path.display())),
    }
}

pub fn save(config: &Config) -> Result<(), String> {
    write_private(
        "config.json",
        &serde_json::to_string_pretty(config).expect("serializes"),
    )
}

/// Creates the config directory, readable only by the user.
fn create_private_dir(dir: &std::path::Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("couldn't restrict {}: {e}", dir.display()))?;
    }
    Ok(())
}

/// Writes a file in the config directory readable only by the user, replacing it atomically.
fn write_private(name: &str, contents: &str) -> Result<(), String> {
    let dir = dir();
    create_private_dir(&dir)?;
    let path = dir.join(name);
    let temp = dir.join(format!(".{name}.{}", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp)
        .map_err(|e| format!("couldn't write {}: {e}", temp.display()))?;
    file.write_all(contents.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("couldn't write {}: {e}", temp.display()))?;
    fs::rename(&temp, &path).map_err(|e| format!("couldn't write {}: {e}", path.display()))
}

/// Held while reading and writing tokens, so two commands refreshing at once don't both redeem
/// the same refresh token (the second would revoke the sign-in).
pub struct Lock(#[allow(dead_code)] File);

pub fn lock() -> Result<Lock, String> {
    let dir = dir();
    create_private_dir(&dir)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(".lock"))
        .map_err(|e| format!("couldn't open the config lock: {e}"))?;
    file.lock()
        .map_err(|e| format!("couldn't lock the config: {e}"))?;
    Ok(Lock(file))
}

pub fn save_tokens(profile: &str, tokens: &Tokens) -> Result<(), String> {
    let mut all = file_tokens()?;
    all.insert(profile.to_owned(), tokens.clone());
    write_private(
        "credentials.json",
        &serde_json::to_string_pretty(&all).expect("serializes"),
    )
}

pub fn load_tokens(profile: &str) -> Result<Option<Tokens>, String> {
    Ok(file_tokens()?.remove(profile))
}

pub fn delete_tokens(profile: &str) -> Result<(), String> {
    remove_file_tokens(profile)
}

fn remove_file_tokens(profile: &str) -> Result<(), String> {
    let mut all = file_tokens()?;
    if all.remove(profile).is_some() {
        write_private(
            "credentials.json",
            &serde_json::to_string_pretty(&all).expect("serializes"),
        )?;
    }
    Ok(())
}

fn file_tokens() -> Result<BTreeMap<String, Tokens>, String> {
    let path = dir().join("credentials.json");
    match fs::read_to_string(&path) {
        Ok(text) => {
            serde_json::from_str(&text).map_err(|e| format!("{} is malformed: {e}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(format!("couldn't read {}: {e}", path.display())),
    }
}
