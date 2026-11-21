//! Reads the command line into a command and its inputs, and runs it.

use crate::auth::{self, LoginOptions};
use crate::client::{self, Auth, Client, MultipartValue, Payload, Request};
use crate::config;
use crate::error::CliError;
use crate::manifest::{self, Operation, ResponseKind, manifest};
use crate::output::{self, Style};
use crate::params::{self, FILE_MARKER, Input, Params};
use crate::table::{self, Field, Table};
use crate::{complete, help};
use serde_json::{Map, Value};
use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::time::Duration;

/// Flags every command takes. `value` is a placeholder for help, set when the flag takes one.
pub struct GlobalFlag {
    pub name: &'static str,
    pub short: Option<char>,
    pub value: Option<&'static str>,
    pub help: &'static str,
}

pub const GLOBAL_FLAGS: &[GlobalFlag] = &[
    GlobalFlag {
        name: "profile",
        short: None,
        value: Some("name"),
        help: "Use a saved sign-in other than the default (see `bias auth status`)",
    },
    GlobalFlag {
        name: "live",
        short: None,
        value: None,
        help: "Allow changes to a live account",
    },
    GlobalFlag {
        name: "api-key",
        short: None,
        value: Some("key"),
        help: "Authenticate with an API key instead of a sign-in (or set BIAS_API_KEY)",
    },
    GlobalFlag {
        name: "api-url",
        short: None,
        value: Some("url"),
        help: "Send requests to another API host (or set BIAS_API_URL)",
    },
    GlobalFlag {
        name: "idempotency-key",
        short: None,
        value: Some("key"),
        help: "Send this Idempotency-Key instead of a generated one",
    },
    GlobalFlag {
        name: "all",
        short: None,
        value: None,
        help: "Fetch every page of a list, printing one object per line",
    },
    GlobalFlag {
        name: "follow",
        short: None,
        value: None,
        help: "Keep a list open and print new objects as they're created",
    },
    GlobalFlag {
        name: "compact",
        short: None,
        value: None,
        help: "Print JSON on one line",
    },
    GlobalFlag {
        name: "table",
        short: Some('t'),
        value: None,
        help: "Print rows and columns for people, with dollars and dates (or set BIAS_OUTPUT=table)",
    },
    GlobalFlag {
        name: "fields",
        short: None,
        value: Some("a,b.c"),
        help: "Show only these fields of the response, in JSON or as --table columns",
    },
    GlobalFlag {
        name: "output",
        short: Some('o'),
        value: Some("file"),
        help: "Write a file response, such as a PDF, to this path",
    },
    GlobalFlag {
        name: "dry-run",
        short: None,
        value: None,
        help: "Print the request instead of sending it",
    },
    GlobalFlag {
        name: "help",
        short: Some('h'),
        value: None,
        help: "Show help for the command",
    },
];

#[derive(Default)]
pub struct Globals {
    pub profile: Option<String>,
    pub live: bool,
    pub api_key: Option<String>,
    pub api_url: Option<String>,
    pub idempotency_key: Option<String>,
    pub all: bool,
    pub follow: bool,
    pub compact: bool,
    pub table: bool,
    pub fields: Option<String>,
    pub output: Option<String>,
    pub dry_run: bool,
    pub help: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Flag { name: String, value: Option<String> },
    Word(String),
}

fn global(name: &str) -> Option<&'static GlobalFlag> {
    GLOBAL_FLAGS.iter().find(|f| f.name == name)
}

/// Splits argv into global flags and the rest. Command-specific flags keep `value: None` when
/// written `--flag value`; the command's schema decides later whether the next word is its value.
pub fn tokenize(args: &[String]) -> Result<(Globals, Vec<Token>), CliError> {
    let mut globals = Globals {
        table: std::env::var("BIAS_OUTPUT").as_deref() == Ok("table"),
        ..Globals::default()
    };
    let mut tokens = Vec::new();
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            tokens.extend(iter.by_ref().map(|a| Token::Word(a.clone())));
            break;
        }
        let (name, value) = if let Some(flag) = arg.strip_prefix("--") {
            match flag.split_once('=') {
                Some((n, v)) => (n.to_owned(), Some(v.to_owned())),
                None => (flag.to_owned(), None),
            }
        } else if let Some(short) = arg.strip_prefix('-').filter(|s| s.len() == 1)
            && let Some(flag) = GLOBAL_FLAGS
                .iter()
                .find(|f| f.short == short.chars().next())
        {
            (flag.name.to_owned(), None)
        } else {
            tokens.push(Token::Word(arg.clone()));
            continue;
        };

        let Some(flag) = global(&name) else {
            tokens.push(Token::Flag { name, value });
            continue;
        };
        let value = match (flag.value, value) {
            (Some(_), Some(v)) => Some(v),
            (Some(placeholder), None) => Some(
                iter.next()
                    .cloned()
                    .ok_or_else(|| CliError::Usage(format!("--{name} needs a {placeholder}")))?,
            ),
            (None, Some(v)) => match v.as_str() {
                "true" => None,
                "false" => continue,
                _ => return Err(CliError::Usage(format!("--{name} doesn't take a value"))),
            },
            (None, None) => None,
        };
        match flag.name {
            "profile" => globals.profile = value,
            "live" => globals.live = true,
            "api-key" => globals.api_key = value,
            "api-url" => globals.api_url = value,
            "idempotency-key" => globals.idempotency_key = value,
            "all" => globals.all = true,
            "follow" => globals.follow = true,
            "compact" => globals.compact = true,
            "table" => globals.table = true,
            "fields" => globals.fields = value,
            "output" => globals.output = value,
            "dry-run" => globals.dry_run = true,
            "help" => globals.help = true,
            _ => unreachable!(),
        }
    }
    Ok((globals, tokens))
}

/// What the leading words name.
pub enum Resolved<'m> {
    Builtin(&'static str),
    Operation(&'m Operation),
    /// The start of several commands, such as `customers`.
    Group(Vec<String>),
    Raw(&'static str),
    Unknown(String),
}

pub const BUILTINS: &[(&str, &str)] = &[
    (
        "login",
        "Sign in with the dashboard (alias of `auth login`)",
    ),
    ("logout", "Sign out (alias of `auth logout`)"),
    (
        "auth",
        "Sign in, sign out and switch between saved sign-ins",
    ),
    (
        "listen",
        "Print events as they happen, and optionally forward them to a local URL",
    ),
    ("open", "Open the dashboard in the browser"),
    ("completion", "Print the shell completion script"),
    ("help", "Show help"),
    ("version", "Print the version"),
];

pub const AUTH_COMMANDS: &[(&str, &str)] = &[
    ("login", "Sign in with the dashboard"),
    ("logout", "Sign out and revoke the sign-in"),
    ("status", "List saved sign-ins"),
    ("switch", "Make a saved sign-in the default"),
    (
        "token",
        "Print an access token for the sign-in, for use with curl",
    ),
];

pub const RAW_METHODS: &[&str] = &["get", "post", "patch", "put", "delete"];

fn normalize(word: &str) -> String {
    let word = word.replace('_', "-").to_lowercase();
    if word == "retrieve" {
        "get".into()
    } else {
        word
    }
}

/// Resolves the command the leading words name; returns it and how many words it used.
pub fn resolve(words: &[String]) -> (Resolved<'static>, usize) {
    let Some(first) = words.first() else {
        return (Resolved::Builtin("help"), 0);
    };
    if let Some((name, _)) = BUILTINS.iter().find(|(n, _)| *n == first.as_str()) {
        return (Resolved::Builtin(name), 1);
    }
    if first == "whoami" {
        return (Resolved::Builtin("whoami"), 1);
    }
    if let Some(method) = RAW_METHODS.iter().find(|m| **m == first.as_str())
        && words.get(1).is_some_and(|w| w.starts_with('/'))
    {
        return (Resolved::Raw(method), 1);
    }

    let normalized: Vec<String> = words.iter().map(|w| normalize(w)).collect();
    let mut best: Option<&Operation> = None;
    for op in &manifest().operations {
        if op.command.len() <= normalized.len()
            && op.command.iter().zip(&normalized).all(|(a, b)| a == b)
            && best.is_none_or(|b| op.command.len() > b.command.len())
        {
            best = Some(op);
        }
    }
    // `customers` alone, or `checkout-sessions discount-code`, names a group.
    let mut depth = 0;
    while depth < normalized.len()
        && manifest()
            .operations
            .iter()
            .any(|op| op.command.len() > depth + 1 && op.command[..=depth] == normalized[..=depth])
    {
        depth += 1;
    }
    match best {
        Some(op) if op.command.len() >= depth => (Resolved::Operation(op), op.command.len()),
        _ if depth > 0 => (Resolved::Group(normalized[..depth].to_vec()), depth),
        _ => (Resolved::Unknown(first.clone()), 1),
    }
}

pub fn run(args: Vec<String>) -> Result<(), CliError> {
    if args.first().map(String::as_str) == Some("__complete") {
        complete::run(&args[1..]);
        return Ok(());
    }
    let (globals, tokens) = tokenize(&args)?;
    let leading: Vec<String> = tokens
        .iter()
        .map_while(|t| match t {
            Token::Word(w) => Some(w.clone()),
            Token::Flag { .. } => None,
        })
        .collect();
    let (resolved, used) = resolve(&leading);
    let rest = &tokens[used..];

    match resolved {
        Resolved::Builtin("help") => {
            let topic: Vec<String> = rest
                .iter()
                .filter_map(|t| match t {
                    Token::Word(w) => Some(w.clone()),
                    _ => None,
                })
                .collect();
            help::topic(&topic)
        }
        _ if globals.help => help::topic(&leading[..used.min(leading.len())]),
        Resolved::Builtin("version") => {
            println!("bias {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Resolved::Builtin("completion") => {
            complete::script(words(rest).first().map(String::as_str))
        }
        Resolved::Builtin("login") => login(&globals, rest),
        Resolved::Builtin("logout") => logout(&globals, rest),
        Resolved::Builtin("whoami") => auth::status(),
        Resolved::Builtin("auth") => auth_command(&globals, rest),
        Resolved::Builtin("open") => {
            let api_url = api_url(&globals);
            let path = words(rest).first().cloned().unwrap_or_default();
            let url = format!(
                "{}/{}",
                auth::dashboard_url(&api_url),
                path.trim_start_matches('/')
            );
            if !auth::open_browser(&url) {
                println!("{url}");
            }
            Ok(())
        }
        Resolved::Builtin("listen") => listen(&globals, rest),
        Resolved::Builtin(other) => unreachable!("builtin {other}"),
        Resolved::Raw(method) => raw(&globals, method, rest),
        Resolved::Operation(op) => operation(&globals, op, rest),
        Resolved::Group(prefix) => match words(rest).first() {
            None => {
                help::group(&prefix);
                Ok(())
            }
            Some(next) => {
                let children: Vec<String> = manifest()
                    .operations
                    .iter()
                    .filter(|op| op.command.len() > prefix.len() && op.command.starts_with(&prefix))
                    .map(|op| op.command[prefix.len()].clone())
                    .collect();
                let mut names = children.clone();
                if children.iter().any(|c| c == "get") {
                    names.push("retrieve".into());
                }
                let group = prefix.join(" ");
                let close = params::suggest(&normalize(next), &names)
                    .or_else(|| params::suggest(next, &names))
                    .map(|c| if c == "retrieve" { "get" } else { c });
                Err(CliError::Usage(match close {
                    Some(close) => format!(
                        "unknown command `{group} {next}`. Did you mean `bias {group} {close}`?"
                    ),
                    None => format!(
                        "unknown command `{group} {next}`. Run `bias help {group}` for its commands."
                    ),
                }))
            }
        },
        Resolved::Unknown(_) => Err(unknown_command(&leading)),
    }
}

fn unknown_command(words: &[String]) -> CliError {
    let typed = words.join(" ");
    let mut names: Vec<String> = BUILTINS.iter().map(|(n, _)| (*n).to_owned()).collect();
    names.extend(manifest().operations.iter().map(|o| o.command.join(" ")));
    names.dedup();
    let first_words: Vec<String> = {
        let mut v: Vec<String> = manifest()
            .operations
            .iter()
            .map(|o| o.command[0].clone())
            .collect();
        v.extend(BUILTINS.iter().map(|(n, _)| (*n).to_owned()));
        v.sort();
        v.dedup();
        v
    };
    let suggestion = params::suggest(&typed, &names).or_else(|| {
        words
            .first()
            .and_then(|w| params::suggest(&normalize(w), &first_words))
    });
    match suggestion {
        Some(s) => CliError::Usage(format!(
            "unknown command `{typed}`. Did you mean `bias {s}`?"
        )),
        None => CliError::Usage(format!("unknown command `{typed}`")),
    }
}

fn words(tokens: &[Token]) -> Vec<String> {
    tokens
        .iter()
        .filter_map(|t| match t {
            Token::Word(w) => Some(w.clone()),
            _ => None,
        })
        .collect()
}

fn flag_set(tokens: &[Token], name: &str) -> Result<bool, CliError> {
    let mut found = false;
    for token in tokens {
        if let Token::Flag { name: n, value } = token {
            if n != name {
                continue;
            }
            match value.as_deref() {
                None | Some("true") => found = true,
                Some("false") => found = false,
                Some(_) => return Err(CliError::Usage(format!("--{name} doesn't take a value"))),
            }
        }
    }
    Ok(found)
}

fn only_flags(tokens: &[Token], allowed: &[&str]) -> Result<(), CliError> {
    for token in tokens {
        match token {
            Token::Flag { name, .. } if !allowed.contains(&name.as_str()) => {
                return Err(CliError::Usage(format!("unknown flag --{name}")));
            }
            _ => {}
        }
    }
    Ok(())
}

fn api_url(globals: &Globals) -> String {
    globals
        .api_url
        .clone()
        .or_else(|| std::env::var("BIAS_API_URL").ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| manifest().base_url.clone())
        .trim_end_matches('/')
        .to_owned()
}

fn profile_name(globals: &Globals) -> Result<String, CliError> {
    if let Some(name) = &globals.profile {
        return Ok(name.clone());
    }
    if let Ok(name) = std::env::var("BIAS_PROFILE")
        && !name.is_empty()
    {
        return Ok(name);
    }
    Ok(config::load()
        .map_err(CliError::Other)?
        .default_profile_name()
        .to_owned())
}

fn login(globals: &Globals, rest: &[Token]) -> Result<(), CliError> {
    only_flags(rest, &["device", "read-only"])?;
    if !words(rest).is_empty() {
        return Err(CliError::Usage(
            "`bias login` takes no arguments; use --profile <name> to name the sign-in".into(),
        ));
    }
    auth::login(LoginOptions {
        profile: globals
            .profile
            .clone()
            .unwrap_or_else(|| config::DEFAULT_PROFILE.to_owned()),
        device: flag_set(rest, "device")?,
        read_only: flag_set(rest, "read-only")?,
        api_url: api_url(globals),
    })
}

fn logout(globals: &Globals, rest: &[Token]) -> Result<(), CliError> {
    only_flags(rest, &["all"])?;
    let profile = globals.profile.clone();
    auth::logout(profile.as_deref(), flag_set(rest, "all")? || globals.all)
}

fn auth_command(globals: &Globals, rest: &[Token]) -> Result<(), CliError> {
    let words = words(rest);
    let sub = words.first().map(String::as_str);
    let rest: Vec<Token> = rest
        .iter()
        .filter(|t| !matches!(t, Token::Word(w) if Some(w.as_str()) == sub))
        .cloned()
        .collect();
    match sub {
        Some("login") => login(globals, &rest),
        Some("logout") => logout(globals, &rest),
        Some("status") => auth::status(),
        Some("switch") => {
            let name = words.get(1).or(globals.profile.as_ref()).ok_or_else(|| {
                CliError::Usage(
                    "name the profile to switch to: `bias auth switch <profile>`".into(),
                )
            })?;
            auth::switch(name)
        }
        Some("token") => {
            let mut client = client(globals)?;
            if let Auth::ApiKey(key) = &client.auth {
                println!("{key}");
                return Ok(());
            }
            // An authenticated no-op request refreshes the token if it's due.
            let response = client.send(&Request {
                method: "GET",
                path: "/v1/events",
                query: &[("limit".into(), "1".into())],
                payload: &Payload::None,
                idempotency_key: None,
                stream: false,
            })?;
            check(response)?;
            if let Auth::Profile { tokens, .. } = &client.auth {
                println!("{}", tokens.access_token);
            }
            Ok(())
        }
        None => help::topic(&["auth".into()]),
        Some(other) => Err(CliError::Usage(format!("unknown command `auth {other}`"))),
    }
}

pub fn client(globals: &Globals) -> Result<Client, CliError> {
    let api_key = globals
        .api_key
        .clone()
        .or_else(|| std::env::var("BIAS_API_KEY").ok().filter(|v| !v.is_empty()));
    if let Some(key) = api_key {
        return Ok(Client {
            http: client::http(),
            base_url: api_url(globals),
            auth: Auth::ApiKey(key),
        });
    }
    let name = profile_name(globals)?;
    let (profile, tokens) = auth::load_profile(&name)?;
    // `--api-url` overrides a profile's host, so a local sign-in can be pointed elsewhere.
    let base_url = globals
        .api_url
        .clone()
        .unwrap_or_else(|| profile.api_url.clone());
    Ok(Client {
        http: client::http(),
        base_url,
        auth: Auth::Profile {
            name,
            profile,
            tokens,
        },
    })
}

/// Reads flags and positionals into an operation's path parameters and params.
pub struct Parsed {
    pub path: String,
    pub params: Map<String, Value>,
}

fn parse_inputs(op: &Operation, rest: &[Token]) -> Result<Parsed, CliError> {
    let schema = op.params_schema();
    let mut params = Params::new(Some(&schema));
    let mut path_values: Vec<Option<String>> = vec![None; op.path_params.len()];
    let mut positionals = Vec::new();
    let mut flags: Vec<(String, Input)> = Vec::new();

    let mut i = 0;
    while i < rest.len() {
        match &rest[i] {
            Token::Word(w) => positionals.push(w.clone()),
            Token::Flag { name, value } => {
                if let Some(index) = op.path_params.iter().position(|p| &p.name == name) {
                    let value = match value {
                        Some(v) => v.clone(),
                        None => match rest.get(i + 1) {
                            Some(Token::Word(w)) => {
                                i += 1;
                                w.clone()
                            }
                            _ => return Err(CliError::Usage(format!("--{name} needs a value"))),
                        },
                    };
                    path_values[index] = Some(value);
                } else {
                    let input = match value {
                        Some(v) => Input::Text(v.clone()),
                        None => match rest.get(i + 1) {
                            Some(Token::Word(next)) if takes_next(params.schema_at(name), next) => {
                                i += 1;
                                Input::Text(next.clone())
                            }
                            _ => Input::Present,
                        },
                    };
                    flags.push((name.clone(), input));
                }
            }
        }
        i += 1;
    }

    let mut positionals = positionals.into_iter();
    for (slot, param) in path_values.iter_mut().zip(&op.path_params) {
        if slot.is_none() {
            *slot = positionals.next();
        }
        if slot.is_none() {
            return Err(CliError::Usage(format!(
                "missing <{}>: {}",
                param.name,
                help::usage(op)
            )));
        }
    }
    let mut positionals = positionals.peekable();
    if let Some(body) = positionals.next_if(|p| looks_like_body(p)) {
        let value = params::parse_body(&body).map_err(CliError::Usage)?;
        params.merge(value).map_err(CliError::Usage)?;
    }
    if let Some(extra) = positionals.next() {
        return Err(CliError::Usage(format!(
            "unexpected argument `{extra}`: {}",
            help::usage(op)
        )));
    }
    for (name, input) in flags {
        params.set(&name, input).map_err(CliError::Usage)?;
    }

    let missing: Vec<String> = manifest::properties_of(&schema)
        .into_iter()
        .filter(|(name, _, required)| *required && !params.root.contains_key(name))
        .map(|(name, _, _)| format!("--{name}"))
        .collect();
    if !missing.is_empty() {
        return Err(CliError::Usage(format!(
            "missing required {}: {}",
            missing.join(", "),
            help::usage(op)
        )));
    }

    let mut path = op.path.clone();
    for (param, value) in op.path_params.iter().zip(path_values) {
        let value = value.expect("checked above");
        path = path.replace(&format!("{{{}}}", param.name), &encode_segment(&value));
    }
    Ok(Parsed {
        path,
        params: params.root,
    })
}

/// A positional body is an object, a file (`@body.json`), or stdin (`-`).
fn looks_like_body(word: &str) -> bool {
    word.trim_start().starts_with('{') || word.starts_with('@') || word == "-"
}

/// Whether `--flag next` means `next` is the flag's value. A boolean takes only `true` or
/// `false`, so `--expand.customer cus_123` leaves the ID positional.
fn takes_next(schema: Option<&Value>, next: &str) -> bool {
    match schema {
        Some(s) => {
            let kinds = manifest::kinds(s);
            if kinds.contains(&"boolean") {
                matches!(next, "true" | "false") || next.starts_with('{')
            } else {
                true
            }
        }
        None => true,
    }
}

fn encode_segment(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn query_value(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(_) | Value::Number(_) => Some(value.to_string()),
        other => Some(other.to_string()),
    }
}

fn split_params(
    op: Option<&Operation>,
    method: &str,
    params: Map<String, Value>,
) -> (Vec<(String, String)>, Map<String, Value>) {
    let mut query = Vec::new();
    let mut body = Map::new();
    for (key, value) in params {
        let in_query = match op {
            Some(op) => op.is_query_param(&key) || op.body.is_none(),
            None => method == "GET" || method == "DELETE",
        };
        if in_query {
            if let Some(v) = query_value(&value) {
                query.push((key, v));
            }
        } else {
            body.insert(key, value);
        }
    }
    (query, body)
}

fn multipart(body: Map<String, Value>) -> Payload {
    let parts = body
        .into_iter()
        .map(|(key, value)| {
            let part = match &value {
                Value::Object(map) if map.len() == 1 && map.contains_key(FILE_MARKER) => {
                    MultipartValue::File(map[FILE_MARKER].as_str().unwrap_or_default().to_owned())
                }
                Value::String(s) => MultipartValue::Text(s.clone()),
                other => MultipartValue::Text(other.to_string()),
            };
            (key, part)
        })
        .collect();
    Payload::Multipart(parts)
}

fn operation(globals: &Globals, op: &Operation, rest: &[Token]) -> Result<(), CliError> {
    let parsed = parse_inputs(op, rest)?;
    if globals.follow && !op.streamable {
        return Err(CliError::Usage(format!(
            "`bias {}` can't --follow",
            op.command.join(" ")
        )));
    }
    if globals.all && op.response != ResponseKind::List {
        return Err(CliError::Usage(format!(
            "`bias {}` isn't a list, so --all doesn't apply",
            op.command.join(" ")
        )));
    }
    let (query, body) = split_params(Some(op), &op.method, parsed.params);
    let payload = if op.is_multipart() {
        multipart(body)
    } else if op.body.is_some()
        && (!body.is_empty() || op.body.as_ref().is_some_and(|b| b.required))
    {
        Payload::Json(Value::Object(body))
    } else {
        Payload::None
    };
    let idempotency_key = op.idempotent.then(|| {
        globals
            .idempotency_key
            .clone()
            .unwrap_or_else(|| client::random_token(18))
    });
    let request = Request {
        method: &op.method,
        path: &parsed.path,
        query: &query,
        payload: &payload,
        idempotency_key: idempotency_key.as_deref(),
        stream: globals.follow,
    };
    send(
        globals,
        request,
        op.response,
        op.columns.as_deref().unwrap_or_default(),
    )
}

fn raw(globals: &Globals, method: &str, rest: &[Token]) -> Result<(), CliError> {
    let mut params = Params::new(None);
    let mut positionals = Vec::new();
    let mut flags = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        match &rest[i] {
            Token::Word(w) => positionals.push(w.clone()),
            Token::Flag { name, value } => {
                let input = match value {
                    Some(v) => Input::Text(v.clone()),
                    None => match rest.get(i + 1) {
                        Some(Token::Word(_)) if positionals.is_empty() => Input::Present,
                        Some(Token::Word(next)) => {
                            i += 1;
                            Input::Text(next.clone())
                        }
                        _ => Input::Present,
                    },
                };
                flags.push((name.clone(), input));
            }
        }
        i += 1;
    }
    let mut positionals = positionals.into_iter();
    let path = positionals.next().expect("resolve checked the path");
    let path = if path.starts_with("/v1/") || path == "/v1" {
        path
    } else {
        format!("/v1{path}")
    };
    if let Some(body) = positionals.next() {
        params
            .merge(params::parse_body(&body).map_err(CliError::Usage)?)
            .map_err(CliError::Usage)?;
    }
    if let Some(extra) = positionals.next() {
        return Err(CliError::Usage(format!("unexpected argument `{extra}`")));
    }
    for (name, input) in flags {
        params.set(&name, input).map_err(CliError::Usage)?;
    }
    let method = method.to_uppercase();
    let (query, body) = split_params(None, &method, params.root);
    let payload = if body.is_empty() {
        Payload::None
    } else {
        Payload::Json(Value::Object(body))
    };
    let idempotency_key =
        (method == "POST" || method == "PATCH" || method == "PUT" || method == "DELETE").then(
            || {
                globals
                    .idempotency_key
                    .clone()
                    .unwrap_or_else(|| client::random_token(18))
            },
        );
    let request = Request {
        method: &method,
        path: &path,
        query: &query,
        payload: &payload,
        idempotency_key: idempotency_key.as_deref(),
        stream: globals.follow,
    };
    let kind = if globals.all {
        ResponseKind::List
    } else {
        ResponseKind::Json
    };
    send(globals, request, kind, &[])
}

/// Prints responses as JSON, narrowed JSON (`--fields`) or a table (`--table`).
struct Printer {
    style: Style,
    fields: Option<Vec<Field>>,
    table: bool,
    columns: Vec<String>,
    rows: Option<Table>,
    /// What `--expand` named, which `--table` shows in full rather than by ID.
    expanded: Vec<String>,
}

impl Printer {
    fn new(globals: &Globals, columns: &[String]) -> Result<Self, CliError> {
        let fields = globals
            .fields
            .as_deref()
            .map(table::parse_fields)
            .transpose()
            .map_err(CliError::Usage)?;
        Ok(Printer {
            style: Style::stdout(globals.compact),
            fields,
            table: globals.table,
            columns: columns.to_vec(),
            rows: None,
            expanded: Vec::new(),
        })
    }

    /// A whole response.
    fn value(&mut self, value: &Value) {
        if !self.table {
            match &self.fields {
                Some(fields) => output::print(&table::project(value, fields), self.style),
                None => output::print(value, self.style),
            }
            return;
        }
        match table::list_items(value) {
            Some(items) if items.is_empty() => eprintln!("No results."),
            Some(items) => self.rows(items),
            None => table::print_object(value, self.fields.as_deref(), &self.expanded),
        }
    }

    /// One of many objects, from `--all` or `--follow`.
    fn item(&mut self, value: &Value) {
        if self.table {
            self.rows(std::slice::from_ref(value));
            return;
        }
        let style = Style {
            compact: true,
            ..self.style
        };
        match &self.fields {
            Some(fields) => output::print(&table::project(value, fields), style),
            None => output::print(value, style),
        }
    }

    fn rows(&mut self, items: &[Value]) {
        let table = self.rows.get_or_insert_with(|| {
            let columns = match &self.fields {
                Some(fields) => fields.clone(),
                None if !self.columns.is_empty() => table::fields_from(&self.columns),
                None => table::guess_columns(items.first().unwrap_or(&Value::Null)),
            };
            Table::new(columns)
        });
        table.rows(items);
    }
}

fn send(
    globals: &Globals,
    request: Request,
    kind: ResponseKind,
    columns: &[String],
) -> Result<(), CliError> {
    let style = Style::stdout(globals.compact);
    let mut printer = Printer::new(globals, columns)?;
    let expand_query = request.query.iter().filter(|(k, _)| k == "expand");
    for (_, value) in expand_query {
        printer
            .expanded
            .extend(table::expanded_paths(&Value::String(value.clone())));
    }
    if let Payload::Json(Value::Object(body)) = request.payload
        && let Some(expand) = body.get("expand")
    {
        printer.expanded.extend(table::expanded_paths(expand));
    }
    if globals.dry_run {
        let mut out = Map::new();
        out.insert("method".into(), request.method.into());
        out.insert("path".into(), request.path.into());
        if !request.query.is_empty() {
            let query: Map<String, Value> = request
                .query
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect();
            out.insert("query".into(), Value::Object(query));
        }
        match request.payload {
            Payload::None => {}
            Payload::Json(body) => {
                out.insert("body".into(), body.clone());
            }
            Payload::Multipart(parts) => {
                let parts: Map<String, Value> = parts
                    .iter()
                    .map(|(k, v)| {
                        let v = match v {
                            MultipartValue::Text(t) => Value::String(t.clone()),
                            MultipartValue::File(p) => Value::String(format!("@{p}")),
                        };
                        (k.clone(), v)
                    })
                    .collect();
                out.insert("multipart".into(), Value::Object(parts));
            }
        }
        if let Some(key) = request.idempotency_key {
            out.insert("idempotency_key".into(), key.into());
        }
        output::print(&Value::Object(out), style);
        return Ok(());
    }

    let mut client = client(globals)?;
    if request.method != "GET" && client.is_live() && !globals.live {
        return Err(CliError::Usage(format!(
            "{} is a live account. Add --live to {} {} against it.",
            client.account_label(),
            request.method,
            request.path
        )));
    }

    if globals.follow {
        let mut query = request.query.to_vec();
        query.push(("stream".into(), "true".into()));
        return follow(&mut client, request.path, &query, |event| {
            printer.item(event);
            Ok(())
        });
    }

    if kind == ResponseKind::List && globals.all {
        let mut query = request.query.to_vec();
        let mut printed = false;
        loop {
            let response = check(client.send(&Request {
                query: &query,
                ..request
            })?)?;
            let page: Value = read_json(response)?;
            let items = page
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for item in &items {
                printer.item(item);
                printed = true;
            }
            let more = page
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let last = items
                .last()
                .and_then(|i| i.get("id"))
                .and_then(Value::as_str);
            match (more, last) {
                (true, Some(id)) => {
                    query.retain(|(k, _)| k != "starting_after" && k != "ending_before");
                    query.push(("starting_after".into(), id.to_owned()));
                }
                _ => {
                    if !printed && globals.table {
                        eprintln!("No results.");
                    }
                    return Ok(());
                }
            }
        }
    }

    let response = check(client.send(&request)?)?;
    if kind == ResponseKind::Binary {
        let bytes = response
            .bytes()
            .map_err(|e| CliError::Network(format!("the download failed: {e}")))?;
        return match &globals.output {
            Some(path) if path != "-" => {
                std::fs::write(path, &bytes)
                    .map_err(|e| CliError::Other(format!("couldn't write {path}: {e}")))?;
                eprintln!("Saved {} bytes to {path}.", bytes.len());
                Ok(())
            }
            None if std::io::stdout().is_terminal() => Err(CliError::Usage(
                "this returns a file. Save it with --output <path>, or pipe it somewhere".into(),
            )),
            _ => {
                let mut stdout = std::io::stdout().lock();
                let _ = stdout.write_all(&bytes);
                Ok(())
            }
        };
    }
    if response.status().as_u16() == 204 {
        return Ok(());
    }
    let value = read_json(response)?;
    printer.value(&value);
    Ok(())
}

fn read_json(response: reqwest::blocking::Response) -> Result<Value, CliError> {
    let text = response
        .text()
        .map_err(|e| CliError::Network(format!("the response was cut off: {e}")))?;
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text)
        .map_err(|e| CliError::Other(format!("the API sent malformed JSON: {e}")))
}

/// Passes successful responses through and turns the rest into API errors.
pub fn check(
    response: reqwest::blocking::Response,
) -> Result<reqwest::blocking::Response, CliError> {
    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(response);
    }
    let request_id = response
        .headers()
        .get("request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = response
        .text()
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    Err(CliError::Api {
        status,
        body,
        request_id,
    })
}

/// Reads a newline-delimited stream, reconnecting with backoff when it drops.
fn follow(
    client: &mut Client,
    path: &str,
    query: &[(String, String)],
    mut each: impl FnMut(&Value) -> Result<(), CliError>,
) -> Result<(), CliError> {
    let mut seen = std::collections::HashSet::new();
    let mut failures = 0u32;
    loop {
        let response = client.send(&Request {
            method: "GET",
            path,
            query,
            payload: &Payload::None,
            idempotency_key: None,
            stream: true,
        });
        match response.and_then(check) {
            Ok(response) => {
                let mut reader = BufReader::new(response);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let text = line.trim();
                    if text.is_empty() {
                        continue;
                    }
                    let Ok(value) = serde_json::from_str::<Value>(text) else {
                        continue;
                    };
                    failures = 0;
                    if let Some(id) = value.get("id").and_then(Value::as_str)
                        && !seen.insert(id.to_owned())
                    {
                        continue;
                    }
                    each(&value)?;
                }
            }
            Err(CliError::Network(message)) => {
                if failures == 0 {
                    eprintln!("Connection lost ({message}); reconnecting…");
                }
            }
            Err(e) => return Err(e),
        }
        failures += 1;
        std::thread::sleep(Duration::from_secs(u64::from(failures.min(5)) * 2));
    }
}

fn listen(globals: &Globals, rest: &[Token]) -> Result<(), CliError> {
    let op = manifest()
        .operations
        .iter()
        .find(|o| o.command == ["events", "list"])
        .expect("events list is in the manifest");
    let mut forward: Option<String> = None;
    let mut tokens = Vec::new();
    let mut iter = rest.iter().peekable();
    while let Some(token) = iter.next() {
        match token {
            Token::Flag { name, value } if name == "forward-to" => {
                forward = Some(match value {
                    Some(v) => v.clone(),
                    None => match iter.next() {
                        Some(Token::Word(w)) => w.clone(),
                        _ => return Err(CliError::Usage("--forward-to needs a URL".into())),
                    },
                });
            }
            other => tokens.push(other.clone()),
        }
    }
    let parsed = parse_inputs(op, &tokens)?;
    let (mut query, _) = split_params(Some(op), "GET", parsed.params);
    query.push(("stream".into(), "true".into()));
    let mut client = client(globals)?;
    let style = Style::stdout(globals.compact);
    let color = output::stderr_color();
    let forwarder = client::http();
    eprintln!(
        "Listening for events on {}{}. Ctrl-C to stop.",
        client.account_label(),
        forward
            .as_ref()
            .map(|u| format!(", forwarding to {u}"))
            .unwrap_or_default()
    );
    follow(&mut client, &parsed.path, &query, |event| {
        let Some(url) = &forward else {
            output::print(event, style);
            return Ok(());
        };
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("event");
        let id = event.get("id").and_then(Value::as_str).unwrap_or("");
        let status = forwarder
            .post(url)
            .timeout(Duration::from_secs(30))
            .header("Content-Type", "application/json")
            .header("Bias-Event-Type", kind)
            .body(event.to_string())
            .send();
        let outcome = match status {
            Ok(r) if r.status().is_success() => {
                output::paint(output::GREEN, &r.status().as_u16().to_string(), color)
            }
            Ok(r) => output::paint(output::RED, &r.status().as_u16().to_string(), color),
            Err(e) => output::paint(output::RED, &format!("failed: {e}"), color),
        };
        eprintln!("{kind} {id} → {outcome}");
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    fn parse(line: &str) -> Result<(String, Value), CliError> {
        let (_, tokens) = tokenize(&args(line))?;
        let leading: Vec<String> = tokens
            .iter()
            .map_while(|t| match t {
                Token::Word(w) => Some(w.clone()),
                _ => None,
            })
            .collect();
        let (Resolved::Operation(op), used) = resolve(&leading) else {
            panic!("{line} doesn't name an operation");
        };
        let parsed = parse_inputs(op, &tokens[used..])?;
        Ok((parsed.path, Value::Object(parsed.params)))
    }

    fn usage(result: Result<(String, Value), CliError>) -> String {
        match result {
            Err(CliError::Usage(message)) => message,
            Err(_) => panic!("not a usage error"),
            Ok(v) => panic!("parsed: {v:?}"),
        }
    }

    #[test]
    fn global_flags_have_no_clashes_with_params() {
        for op in &manifest().operations {
            for (name, _, _) in manifest::properties_of(&op.params_schema()) {
                assert!(
                    global(&name).is_none() && global(&name.replace('_', "-")).is_none(),
                    "--{name} of `{}` is also a global flag",
                    op.command.join(" ")
                );
            }
        }
    }

    #[test]
    fn resolves_commands() {
        let names = |line: &str| match resolve(&args(line)).0 {
            Resolved::Operation(op) => op.command.join(" "),
            Resolved::Group(words) => format!("group {}", words.join(" ")),
            Resolved::Builtin(b) => format!("builtin {b}"),
            Resolved::Raw(m) => format!("raw {m}"),
            Resolved::Unknown(w) => format!("unknown {w}"),
        };
        assert_eq!(names("customers create"), "customers create");
        assert_eq!(names("customers retrieve cus_1"), "customers get");
        assert_eq!(
            names("checkout_sessions discount_code apply x"),
            "checkout-sessions discount-code apply"
        );
        assert_eq!(names("customers"), "group customers");
        assert_eq!(
            names("checkout-sessions discount-code"),
            "group checkout-sessions discount-code"
        );
        assert_eq!(names("get /v1/customers"), "raw get");
        assert_eq!(names("login"), "builtin login");
        assert_eq!(names("nope"), "unknown nope");
    }

    #[test]
    fn reads_flags_both_ways() {
        let (_, params) = parse("customers create --name=john --email john@example.com").unwrap();
        assert_eq!(
            params,
            serde_json::json!({"name": "john", "email": "john@example.com"})
        );
    }

    #[test]
    fn reads_bodies_and_paths() {
        let (path, params) =
            parse("customers update cus_1 {name:'jo',} --metadata.vip true").unwrap();
        assert_eq!(path, "/v1/customers/cus_1");
        assert_eq!(
            params,
            serde_json::json!({"name": "jo", "metadata": {"vip": true}})
        );
    }

    #[test]
    fn boolean_flags_leave_the_next_word_alone() {
        let (path, params) = parse("payments get --expand.customer pay_1").unwrap();
        assert_eq!(path, "/v1/payments/pay_1");
        assert_eq!(params, serde_json::json!({"expand": {"customer": true}}));
    }

    #[test]
    fn fails_on_missing_inputs() {
        assert!(usage(parse("customers get")).starts_with("missing <id>"));
        assert!(usage(parse("refunds create")).starts_with("missing required --payment"));
        assert!(usage(parse("customers get a b")).starts_with("unexpected argument `b`"));
    }
}
