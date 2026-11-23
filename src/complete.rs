//! Shell completion. Each shell's script calls `bias __complete <shell> -- <words…>`, the last
//! word being the one under the cursor, and gets back `candidate<TAB>description` lines.

use crate::cli::{AUTH_COMMANDS, BUILTINS, GLOBAL_FLAGS, Resolved, Token, resolve, tokenize};
use crate::config;
use crate::error::CliError;
use crate::manifest::{self, Operation, manifest};
use crate::params::Params;
use serde_json::Value;

const ZSH: &str = r#"#compdef bias
# bias completion for zsh. Add to ~/.zshrc:  source <(bias completion zsh)

_bias() {
    local -a out spaced spaced_desc open open_desc
    local line candidate description
    out=("${(@f)$(command ${~words[1]} __complete zsh -- "${(@)words[2,CURRENT]}" 2>/dev/null)}")
    for line in "${out[@]}"; do
        [[ -z $line ]] && continue
        candidate=${line%%$'\t'*}
        description=$candidate
        [[ $line == *$'\t'* ]] && description="$candidate  -- ${line#*$'\t'}"
        if [[ $candidate == *[.=,\[] ]]; then
            open+=("$candidate"); open_desc+=("$description")
        else
            spaced+=("$candidate"); spaced_desc+=("$description")
        fi
    done
    (( ${#spaced} )) && compadd -l -d spaced_desc -- "${spaced[@]}"
    (( ${#open} )) && compadd -l -S '' -d open_desc -- "${open[@]}"
    return 0
}

# Autoloaded from fpath (as Homebrew installs it), this file is _bias's body, so complete now.
# Sourced, register the function.
if [[ $funcstack[1] == _bias ]]; then
    _bias "$@"
else
    compdef _bias bias
fi
# Brackets in --items[0].price would otherwise be a glob.
alias bias='noglob bias'
"#;

const BASH: &str = r#"# bias completion for bash. Add to ~/.bashrc:  eval "$(bias completion bash)"

_bias() {
    local IFS=$'\n' line candidate nospace=
    COMPREPLY=()
    for line in $(command "${COMP_WORDS[0]}" __complete bash -- "${COMP_LINE:0:COMP_POINT}" 2>/dev/null); do
        candidate=${line%%$'\t'*}
        [[ $candidate == *[.=,[] ]] && nospace=1
        COMPREPLY+=("$candidate")
    done
    [[ -n $nospace ]] && compopt -o nospace
    return 0
}

complete -o default -F _bias bias
"#;

const FISH: &str = r#"# bias completion for fish. Add to config.fish:  bias completion fish | source

function __bias_complete
    set -l words (commandline -opc)
    # The binary as typed, so ./bias completes before bias is on PATH.
    set -l bin $words[1]
    command -q $bin; or set bin bias
    command $bin __complete fish -- $words[2..-1] (commandline -ct) 2>/dev/null
end

complete -c bias -f -a '(__bias_complete)'
"#;

pub fn script(shell: Option<&str>) -> Result<(), CliError> {
    match shell {
        Some("zsh") => print!("{ZSH}"),
        Some("bash") => print!("{BASH}"),
        Some("fish") => print!("{FISH}"),
        _ => {
            return Err(CliError::Usage(
                "name the shell: `bias completion zsh`, `bash` or `fish`".into(),
            ));
        }
    }
    Ok(())
}

pub fn run(args: &[String]) {
    let shell = args.first().map(String::as_str).unwrap_or("zsh");
    let words: Vec<String> = match args.iter().position(|a| a == "--") {
        Some(i) => args[i + 1..].to_vec(),
        None => args.get(1..).unwrap_or_default().to_vec(),
    };
    let words = if shell == "bash" {
        split_line(words.first().map(String::as_str).unwrap_or(""))
            .into_iter()
            .skip(1)
            .collect()
    } else {
        words
    };
    let current = words.last().cloned().unwrap_or_default();
    for (candidate, description) in candidates(&words) {
        // Bash splits words at `=` and `:`, so it completes only the part after the last one.
        let candidate = if shell == "bash" {
            let cut = current.rfind(['=', ':']).map_or(0, |i| i + 1);
            candidate.get(cut..).unwrap_or(&candidate).to_owned()
        } else {
            candidate
        };
        let description = description.replace(['\t', '\n'], " ");
        if description.is_empty() {
            println!("{candidate}");
        } else {
            println!("{candidate}\t{description}");
        }
    }
}

/// Splits a bash command line the way the shell would, keeping an empty last word when the line
/// ends in a space.
fn split_line(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '\'' | '"') => quote = Some(c),
            (None, '\\') => {
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            }
            (None, c) if c.is_whitespace() => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            (None, c) => word.push(c),
        }
    }
    words.push(word);
    words
}

pub fn candidates(words: &[String]) -> Vec<(String, String)> {
    let (current, prior) = match words.split_last() {
        Some((current, prior)) => (current.as_str(), prior),
        None => ("", &[][..]),
    };

    // A global flag waiting for its value.
    let waiting = prior
        .last()
        .and_then(|last| last.strip_prefix("--"))
        .and_then(|flag| GLOBAL_FLAGS.iter().find(|f| f.name == flag))
        .filter(|global| global.value.is_some());
    let prior = if waiting.is_some() {
        &prior[..prior.len() - 1]
    } else {
        prior
    };

    let Ok((_, tokens)) = tokenize(prior) else {
        return Vec::new();
    };
    let leading: Vec<String> = tokens
        .iter()
        .map_while(|t| match t {
            Token::Word(w) => Some(w.clone()),
            Token::Flag { .. } => None,
        })
        .collect();
    let (resolved, used) = if leading.is_empty() {
        (Resolved::Group(Vec::new()), 0)
    } else {
        resolve(&leading)
    };
    let complete_command = used == leading.len();

    let op = match &resolved {
        Resolved::Operation(op) => Some(*op),
        Resolved::Builtin("listen") => manifest()
            .operations
            .iter()
            .find(|o| o.command == ["events", "list"]),
        _ => None,
    };
    if let Some(global) = waiting {
        return match global.name {
            "profile" => profile_names(current),
            "fields" => field_candidates(op, current, ""),
            _ => Vec::new(),
        };
    }
    if let Some(list) = current.strip_prefix("--fields=") {
        return field_candidates(op, list, "--fields=");
    }
    if let Some(flag) = current.strip_prefix("--") {
        return flag_candidates(op, flag);
    }

    match resolved {
        Resolved::Group(prefix) if complete_command => next_words(&prefix, current),
        Resolved::Builtin("help") => {
            let words: Vec<String> = leading[1..].to_vec();
            match resolve(&words).0 {
                Resolved::Group(prefix) if !words.is_empty() => next_words(&prefix, current),
                _ if words.is_empty() => next_words(&[], current),
                _ => Vec::new(),
            }
        }
        Resolved::Builtin("auth") if leading.len() == 1 => filter(
            AUTH_COMMANDS
                .iter()
                .map(|(n, d)| ((*n).to_owned(), (*d).to_owned())),
            current,
        ),
        Resolved::Builtin("auth") if leading.get(1).map(String::as_str) == Some("switch") => {
            profile_names(current)
        }
        Resolved::Builtin("completion") if leading.len() == 1 => filter(
            ["zsh", "bash", "fish"].map(|s| (s.to_owned(), String::new())),
            current,
        ),
        Resolved::Operation(op) => value_candidates(op, &tokens[used..], current),
        _ => Vec::new(),
    }
}

fn filter(
    items: impl IntoIterator<Item = (String, String)>,
    prefix: &str,
) -> Vec<(String, String)> {
    items
        .into_iter()
        .filter(|(name, _)| name.starts_with(prefix))
        .collect()
}

fn profile_names(prefix: &str) -> Vec<(String, String)> {
    let Ok(config) = config::load() else {
        return Vec::new();
    };
    filter(
        config
            .profiles
            .iter()
            .map(|(name, p)| (name.clone(), p.account.name.clone())),
        prefix,
    )
}

/// The commands that can follow `prefix`, such as `create` after `customers`.
fn next_words(prefix: &[String], current: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if prefix.is_empty() {
        out.extend(
            BUILTINS
                .iter()
                .map(|(n, d)| ((*n).to_owned(), (*d).to_owned())),
        );
    }
    for op in &manifest().operations {
        if op.command.len() > prefix.len() && op.command.starts_with(prefix) {
            let word = op.command[prefix.len()].clone();
            let description = if op.command.len() == prefix.len() + 1 {
                op.description.clone().unwrap_or_default()
            } else {
                String::new()
            };
            match out.iter_mut().find(|(w, _)| *w == word) {
                Some(existing) => {
                    if existing.1.is_empty() {
                        existing.1 = description;
                    }
                }
                None => out.push((word, description)),
            }
        }
    }
    filter(out, current)
}

/// Completes the last of a comma-separated `--fields` list from the response's schema. Each
/// candidate ends in `.` (more to pick inside) or `,` (ready for the next field).
fn field_candidates(op: Option<&Operation>, list: &str, prefix: &str) -> Vec<(String, String)> {
    let Some(name) = op.and_then(|o| o.response_schema.as_deref()) else {
        return Vec::new();
    };
    let schema = serde_json::json!({ "$ref": name });
    let params = Params::new(Some(&schema));
    let (done, current) = match list.rfind(',') {
        Some(i) => (&list[..=i], &list[i + 1..]),
        None => ("", list),
    };
    let (parent, partial) = match current.rfind('.') {
        Some(i) => (&current[..i], &current[i + 1..]),
        None => ("", current),
    };
    let Some(parent_schema) = (if parent.is_empty() {
        Some(&schema)
    } else {
        params.schema_at(parent)
    }) else {
        return Vec::new();
    };
    let target = if manifest::kinds(parent_schema).contains(&"array") {
        manifest::items(parent_schema).unwrap_or(parent_schema)
    } else {
        parent_schema
    };
    manifest::properties_of(target)
        .into_iter()
        .filter(|(n, _, _)| n.starts_with(partial))
        .map(|(n, child, _)| {
            let path = if parent.is_empty() {
                n.clone()
            } else {
                format!("{parent}.{n}")
            };
            let kinds = manifest::kinds(child);
            let nested = !manifest::properties_of(child).is_empty()
                && !kinds
                    .iter()
                    .any(|k| matches!(*k, "string" | "integer" | "number" | "boolean"));
            let suffix = if nested { "." } else { "," };
            let description = manifest::description(child).unwrap_or_default().to_owned();
            (format!("{prefix}{done}{path}{suffix}"), description)
        })
        .collect()
}

/// Completes `--a.b` paths from the schema, and `--a=` values from enums.
fn flag_candidates(op: Option<&Operation>, flag: &str) -> Vec<(String, String)> {
    let schema = op.map(Operation::params_schema);
    let params = Params::new(schema.as_ref());

    if let Some((name, prefix)) = flag.split_once('=') {
        return enum_candidates(params.schema_at(name))
            .into_iter()
            .filter(|(v, _)| v.starts_with(prefix))
            .map(|(v, d)| (format!("--{name}={v}"), d))
            .collect();
    }

    let cut = flag.rfind(['.', '[']);
    let (parent, partial) = match cut {
        Some(i) if flag.as_bytes()[i] == b'.' => (&flag[..i], &flag[i + 1..]),
        Some(i) => (&flag[..i], &flag[i..]),
        None => ("", flag),
    };
    let mut out = Vec::new();
    if parent.is_empty() {
        out.extend(
            GLOBAL_FLAGS
                .iter()
                .filter(|f| f.name.starts_with(partial))
                .map(|f| {
                    let suffix = if f.value.is_some() { "=" } else { "" };
                    (format!("--{}{suffix}", f.name), f.help.to_owned())
                }),
        );
        if op.is_some_and(|o| o.command == ["events", "list"]) && "forward-to".starts_with(partial)
        {
            out.push(("--forward-to=".into(), "POST each event to this URL".into()));
        }
    }
    let Some(parent_schema) = (if parent.is_empty() {
        schema.as_ref()
    } else {
        params.schema_at(parent)
    }) else {
        return out;
    };
    if partial.starts_with('[') {
        return if manifest::kinds(parent_schema).contains(&"array") {
            vec![(format!("--{parent}[0]."), String::new())]
        } else {
            out
        };
    }
    for (name, child, required) in manifest::properties_of(parent_schema) {
        if !name.starts_with(partial) {
            continue;
        }
        let path = if parent.is_empty() {
            name.clone()
        } else {
            format!("{parent}.{name}")
        };
        let kinds = manifest::kinds(child);
        let scalar = kinds
            .iter()
            .any(|k| matches!(*k, "string" | "integer" | "number" | "any"));
        let suffix = if kinds.contains(&"boolean") {
            ""
        } else if kinds.contains(&"object") && !scalar {
            "."
        } else if kinds.contains(&"array")
            && manifest::items(child).is_some_and(|i| manifest::kinds(i).contains(&"object"))
        {
            "[0]."
        } else {
            "="
        };
        let mut description = manifest::description(child).unwrap_or_default().to_owned();
        if required {
            description = format!("Required. {description}");
        }
        out.push((format!("--{path}{suffix}"), description));
    }
    out
}

fn enum_candidates(schema: Option<&Value>) -> Vec<(String, String)> {
    let Some(schema) = schema else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = manifest::enum_values(schema)
        .unwrap_or_default()
        .into_iter()
        .map(|v| (v, String::new()))
        .collect();
    let kinds = manifest::kinds(schema);
    if kinds.contains(&"boolean") {
        out.extend(["true", "false"].map(|v| (v.to_owned(), String::new())));
    }
    if kinds.contains(&"null") && !out.is_empty() {
        out.push(("null".into(), String::new()));
    }
    out
}

/// A value for the flag just before the cursor, written `--flag value`.
fn value_candidates(op: &Operation, tokens: &[Token], current: &str) -> Vec<(String, String)> {
    let Some(Token::Flag { name, value: None }) = tokens.last() else {
        return Vec::new();
    };
    let schema = op.params_schema();
    let params = Params::new(Some(&schema));
    filter(enum_candidates(params.schema_at(name)), current)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(line: &str) -> Vec<String> {
        let mut words: Vec<String> = line.split(' ').map(str::to_owned).collect();
        if line.is_empty() {
            words = vec![String::new()];
        }
        candidates(&words).into_iter().map(|(c, _)| c).collect()
    }

    #[test]
    fn completes_commands() {
        assert!(names("cust").contains(&"customers".to_owned()));
        assert!(names("customers ").contains(&"create".to_owned()));
        assert_eq!(names("checkout-sessions discount-code a"), vec!["apply"]);
        assert_eq!(names("auth sw"), vec!["switch"]);
    }

    #[test]
    fn completes_nested_flags() {
        let top = names("customers create --sh");
        assert_eq!(top, vec!["--shipping_details."]);
        let nested = names("customers create --shipping_details.address.li");
        assert_eq!(
            nested,
            vec![
                "--shipping_details.address.line1=",
                "--shipping_details.address.line2="
            ]
        );
        assert!(names("invoices create --line_").contains(&"--line_items[0].".to_owned()));
        assert!(
            names("invoices create --line_items[0].q")
                .contains(&"--line_items[0].quantity=".to_owned())
        );
    }

    #[test]
    fn completes_enum_values() {
        assert_eq!(
            names("customers create --tax_exempt="),
            vec!["--tax_exempt=none", "--tax_exempt=exempt"]
        );
        assert_eq!(names("customers create --tax_exempt e"), vec!["exempt"]);
    }

    #[test]
    fn completes_response_fields() {
        assert!(names("payments list --fields=am").contains(&"--fields=amount,".to_owned()));
        assert!(names("payments list --fields=id,ri").contains(&"--fields=id,risk.".to_owned()));
        assert!(names("payments list --fields id,risk.le").contains(&"id,risk.level,".to_owned()));
    }

    #[test]
    fn splits_bash_lines() {
        assert_eq!(
            split_line("bias customers create --name='a b' "),
            vec!["bias", "customers", "create", "--name=a b", ""]
        );
    }
}
