//! Help text, built from the manifest so it matches the API.

use crate::cli::{AUTH_COMMANDS, BUILTINS, GLOBAL_FLAGS, Resolved, resolve};
use crate::error::CliError;
use crate::manifest::{self, Operation, manifest};
use crate::output::{self, BOLD, DIM, paint};
use std::collections::BTreeMap;

fn color() -> bool {
    output::stdout_color()
}

fn heading(text: &str) -> String {
    paint(BOLD, text, color())
}

pub fn topic(words: &[String]) -> Result<(), CliError> {
    if words.is_empty() {
        root();
        return Ok(());
    }
    if words[0] == "auth" {
        auth();
        return Ok(());
    }
    match resolve(words).0 {
        Resolved::Operation(op) => operation(op),
        Resolved::Group(prefix) => group(&prefix),
        Resolved::Builtin("login") => login(),
        Resolved::Builtin("listen") => listen(),
        Resolved::Builtin("completion") => completion(),
        Resolved::Builtin(_) => root(),
        Resolved::Unknown(word) => {
            return Err(CliError::Usage(format!("no help for `{word}`")));
        }
    }
    Ok(())
}

/// Characters a terminal shows, leaving out color codes.
fn visible_len(text: &str) -> usize {
    let mut len = 0;
    let mut escape = false;
    for c in text.chars() {
        match (escape, c) {
            (false, '\x1b') => escape = true,
            (true, 'm') => escape = false,
            (true, _) => {}
            (false, _) => len += 1,
        }
    }
    len
}

fn rows(rows: &[(String, String)]) {
    let width = rows
        .iter()
        .map(|(a, _)| visible_len(a))
        .max()
        .unwrap_or(0)
        .min(36);
    for (name, description) in rows {
        let pad = width.saturating_sub(visible_len(name));
        if description.is_empty() {
            println!("  {name}");
        } else if visible_len(name) > width {
            println!(
                "  {name}\n  {}{}",
                " ".repeat(width + 2),
                paint(DIM, description, color())
            );
        } else {
            println!(
                "  {name}{}  {}",
                " ".repeat(pad),
                paint(DIM, description, color())
            );
        }
    }
}

fn root() {
    println!("Work with the Bias API from the command line.\n");
    println!(
        "{}\n  bias <command> [arguments] [--flags]\n",
        heading("Usage")
    );
    println!("{}", heading("Commands"));
    rows(
        &BUILTINS
            .iter()
            .map(|(n, d)| ((*n).to_owned(), (*d).to_owned()))
            .collect::<Vec<_>>(),
    );
    println!("\n{}", heading("Resources"));
    let mut resources: BTreeMap<&str, usize> = BTreeMap::new();
    for op in &manifest().operations {
        *resources.entry(op.command[0].as_str()).or_default() += 1;
    }
    let list: Vec<(String, String)> = resources
        .into_iter()
        .map(|(name, count)| {
            (
                name.to_owned(),
                format!("{count} command{}", if count == 1 { "" } else { "s" }),
            )
        })
        .collect();
    rows(&list);
    println!("\n{}", heading("Input"));
    println!("  bias customers create --name=john --email john@example.com");
    println!("  bias customers create '{{name: \"john\", metadata: {{vip: true}},}}'");
    println!("  bias invoices create --customer cus_123 --line_items[0].price=price_123");
    println!("  bias invoices create @new_invoice.json        (or - to read stdin)");
    println!(
        "\n  Values are typed by the API's schema: `--limit 5` sends a number, `--name 5` a string."
    );
    println!(
        "  Nested fields take dots and indexes: --a.b[0].c=3, or --a.b.0.c=3 in zsh without noglob."
    );
    println!(
        "  A value starting with {{ or [ is read as JSON, which may skip quotes on keys and words."
    );
    println!("  @path reads a value from a file; start it with \\@ to send a literal @.");
    println!("\n{}", heading("Output"));
    println!(
        "  JSON by default. -t prints a table, with dollars and local dates; BIAS_OUTPUT=table makes it the default."
    );
    println!(
        "  --fields id,amount,risk.level narrows either; items[].price picks from every item of a list."
    );
    globals();
    println!("\nRun `bias help <command>` for a command's flags.");
}

fn globals() {
    println!("\n{}", heading("Global flags"));
    let list: Vec<(String, String)> = GLOBAL_FLAGS
        .iter()
        .filter(|f| f.name != "help")
        .map(|f| {
            let short = f.short.map(|c| format!("-{c}, ")).unwrap_or_default();
            let value = f.value.map(|v| format!(" <{v}>")).unwrap_or_default();
            (format!("{short}--{}{value}", f.name), f.help.to_owned())
        })
        .collect();
    rows(&list);
}

pub fn group(prefix: &[String]) {
    println!("{}\n", heading(&format!("bias {}", prefix.join(" "))));
    let list: Vec<(String, String)> = manifest()
        .operations
        .iter()
        .filter(|op| op.command.starts_with(prefix))
        .map(|op| {
            (
                usage_words(op, false)[prefix.len()..].join(" "),
                op.description.clone().unwrap_or_default(),
            )
        })
        .collect();
    rows(&list);
    println!(
        "\nRun `bias help {} <command>` for a command's flags.",
        prefix.join(" ")
    );
}

fn usage_words(op: &Operation, flags: bool) -> Vec<String> {
    let mut words: Vec<String> = op.command.clone();
    words.extend(op.path_params.iter().map(|p| format!("<{}>", p.name)));
    if flags && op.body.is_some() {
        words.push("[body]".into());
    }
    if flags {
        words.push("[--flags]".into());
    }
    words
}

pub fn usage(op: &Operation) -> String {
    format!("bias {}", usage_words(op, true).join(" "))
}

fn operation(op: &Operation) {
    if let Some(description) = &op.description {
        println!("{description}\n");
    }
    println!("{}\n  {}", heading("Usage"), usage(op));
    println!(
        "  {}",
        paint(DIM, &format!("{} {}", op.method, op.path), color())
    );
    if !op.path_params.is_empty() {
        println!("\n{}", heading("Arguments"));
        let list: Vec<(String, String)> = op
            .path_params
            .iter()
            .map(|p| {
                (
                    format!("<{}>", p.name),
                    p.description.clone().unwrap_or_default(),
                )
            })
            .collect();
        rows(&list);
    }
    let schema = op.params_schema();
    let properties = manifest::properties_of(&schema);
    if !properties.is_empty() {
        println!("\n{}", heading("Flags"));
        let mut list: Vec<(String, String)> = Vec::new();
        let mut sorted = properties;
        sorted.sort_by_key(|(name, _, required)| (!required, name.clone()));
        for (name, child, required) in sorted {
            let label = manifest::type_label(child);
            let mut description = manifest::description(child).unwrap_or_default().to_owned();
            if required {
                description = format!("Required. {description}");
            }
            list.push((
                format!("--{name} {}", paint(DIM, &format!("<{label}>"), color())),
                description,
            ));
            if let Some(fields) = nested_fields(child) {
                list.push((format!("    .{fields}"), String::new()));
            }
        }
        rows(&list);
    }
    if op.body.is_some() {
        println!(
            "\n  The [body] is JSON or loose JSON, @file, or - for stdin; flags are merged over it."
        );
    }
    if op.response == manifest::ResponseKind::List {
        println!(
            "  --all fetches every page{}.",
            if op.streamable {
                "; --follow prints new ones as they come"
            } else {
                ""
            }
        );
    }
    if let Some(columns) = &op.columns
        && op.response == manifest::ResponseKind::List
    {
        println!(
            "  --table shows {}; --fields picks others.",
            columns.join(", ")
        );
    }
    if op.response == manifest::ResponseKind::Binary {
        println!("  Returns a file: save it with --output <path>.");
    }
}

/// `{a, b, c}` for an object's fields, so nested flags are discoverable.
fn nested_fields(schema: &serde_json::Value) -> Option<String> {
    let kinds = manifest::kinds(schema);
    let target = if kinds.contains(&"object") {
        schema
    } else if kinds.contains(&"array") {
        manifest::items(schema)?
    } else {
        return None;
    };
    let names: Vec<String> = manifest::properties_of(target)
        .into_iter()
        .map(|(n, _, _)| n)
        .collect();
    if names.is_empty() {
        return None;
    }
    let prefix = if kinds.contains(&"object") { "" } else { "[0]" };
    let shown = if names.len() > 8 {
        format!("{}, …", names[..8].join(", "))
    } else {
        names.join(", ")
    };
    Some(
        format!("{prefix}{{{shown}}}")
            .trim_start_matches('.')
            .to_owned(),
    )
}

fn auth() {
    println!("{}\n  bias auth <command>\n", heading("Usage"));
    rows(
        &AUTH_COMMANDS
            .iter()
            .map(|(n, d)| ((*n).to_owned(), (*d).to_owned()))
            .collect::<Vec<_>>(),
    );
    println!("\nEach sign-in is saved as a profile: `bias login --profile live`, then");
    println!("`bias --profile live …` or `bias auth switch live`. BIAS_API_KEY skips sign-in.");
}

fn login() {
    println!("Sign in with the dashboard. Opens the browser, or shows a code to enter there.\n");
    println!(
        "{}\n  bias login [--device] [--read-only] [--profile <name>]\n",
        heading("Usage")
    );
    rows(&[
        (
            "--device".into(),
            "Sign in with a code instead of a browser redirect, e.g. over SSH".into(),
        ),
        (
            "--read-only".into(),
            "Allow only GET requests with this sign-in".into(),
        ),
        (
            "--profile <name>".into(),
            "Save the sign-in under this name (default: default)".into(),
        ),
    ]);
}

fn listen() {
    println!("Print events as they're created, or forward each to a URL as a POST.\n");
    println!(
        "{}\n  bias listen [--forward-to <url>] [events list flags]\n",
        heading("Usage")
    );
    println!("  bias listen --forward-to http://localhost:3000/webhooks");
    println!("  bias listen --filters '{{type: \"payment.succeeded\"}}'");
}

fn completion() {
    println!("Print a completion script for your shell.\n");
    println!("  zsh   source <(bias completion zsh)      in ~/.zshrc");
    println!("  bash  eval \"$(bias completion bash)\"     in ~/.bashrc");
    println!("  fish  bias completion fish | source      in ~/.config/fish/config.fish");
    println!("\nThe zsh script also runs bias under noglob, so --items[0].price needs no quotes.");
}
