use crate::output;
use serde_json::Value;

#[derive(Debug)]
pub enum CliError {
    /// The command line is wrong: exit 2.
    Usage(String),
    /// A sign-in that failed: denied, timed out, or refused.
    Auth(String),
    /// Not signed in, or the sign-in ended. Not a failure, so no `error:`: what happened, then the
    /// command that fixes it.
    SignedOut {
        message: String,
        profile: String,
    },
    Network(String),
    /// The API answered with an error.
    Api {
        status: u16,
        body: Value,
        request_id: Option<String>,
    },
    Other(String),
}

impl CliError {
    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::Usage(_) => 2,
            _ => 1,
        }
    }

    pub fn print(&self) {
        let red = |s: &str| output::paint(output::RED, s, output::stderr_color());
        match self {
            CliError::Usage(message) => {
                eprintln!("{} {message}", red("error:"));
                eprintln!("Run `bias help` for usage.");
            }
            CliError::SignedOut { message, profile } => {
                let color = output::stderr_color();
                let command = format!("bias login{}", crate::client::profile_flag(profile));
                eprintln!("{message}");
                eprintln!(
                    "Run {} to sign in.",
                    output::paint(output::BOLD, &command, color)
                );
            }
            CliError::Auth(message) | CliError::Network(message) | CliError::Other(message) => {
                eprintln!("{} {message}", red("error:"))
            }
            CliError::Api {
                status,
                body,
                request_id,
            } => {
                let error = body.get("error").unwrap_or(body);
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("the API answered {status}"));
                eprintln!("{} {message}", red("error:"));
                for key in ["type", "code", "param", "decline_code"] {
                    if let Some(value) = error.get(key).and_then(Value::as_str) {
                        eprintln!("  {key}: {value}");
                    }
                }
                if let Some(Value::Array(issues)) = error.get("issues") {
                    for issue in issues.iter().take(10) {
                        let path = issue
                            .get("path")
                            .and_then(Value::as_array)
                            .map(|p| {
                                p.iter()
                                    .map(|s| match s {
                                        Value::String(s) => s.clone(),
                                        other => other.to_string(),
                                    })
                                    .collect::<Vec<_>>()
                                    .join(".")
                            })
                            .unwrap_or_default();
                        let message = issue.get("message").and_then(Value::as_str).unwrap_or("");
                        eprintln!("  - {path}: {message}");
                    }
                }
                if let Some(id) = request_id {
                    eprintln!("  request: {id}");
                }
            }
        }
    }
}
