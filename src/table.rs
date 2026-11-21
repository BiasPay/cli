//! `--fields` picks parts of a response, and `--table` prints it for people: rows and columns,
//! dollars instead of cents, dates instead of epoch milliseconds, cut to the terminal's width.

use crate::output::{self, paint};
use crate::params::{Segment, parse_path};
use serde_json::{Map, Value};
use std::io::Write;

/// A response field, e.g. `risk.level` or `line_items[0].price`.
#[derive(Clone)]
pub struct Field {
    pub path: String,
    segments: Vec<Segment>,
}

pub fn parse_fields(list: &str) -> Result<Vec<Field>, String> {
    list.split(',')
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map(|path| {
            parse_path(path)
                .map(|segments| Field {
                    path: path.to_owned(),
                    segments,
                })
                .map_err(|e| format!("--fields: {e}"))
        })
        .collect()
}

pub fn fields_from(paths: &[String]) -> Vec<Field> {
    paths
        .iter()
        .filter_map(|p| parse_fields(p).ok())
        .flatten()
        .collect()
}

/// The value at a path. `[]` gathers the rest of the path from every item of a list.
fn get(value: &Value, segments: &[Segment]) -> Option<Value> {
    let Some((first, rest)) = segments.split_first() else {
        return Some(value.clone());
    };
    match (first, value) {
        (Segment::Key(k) | Segment::Number(k), Value::Object(map)) => get(map.get(k)?, rest),
        (Segment::Number(n), Value::Array(items)) => {
            get(items.get(n.parse::<usize>().ok()?)?, rest)
        }
        (Segment::Index(i), Value::Array(items)) => get(items.get(*i)?, rest),
        (Segment::Append, Value::Array(items)) => Some(Value::Array(
            items.iter().filter_map(|i| get(i, rest)).collect(),
        )),
        _ => None,
    }
}

/// The value pruned to one path, keeping its nesting: `{risk: {level: …}}` for `risk.level`.
fn pick(value: &Value, segments: &[Segment]) -> Option<Value> {
    let Some((first, rest)) = segments.split_first() else {
        return Some(value.clone());
    };
    match (first, value) {
        (Segment::Key(k) | Segment::Number(k), Value::Object(map)) => {
            let mut out = Map::new();
            out.insert(k.clone(), pick(map.get(k)?, rest)?);
            Some(Value::Object(out))
        }
        (Segment::Number(n), Value::Array(items)) => Some(Value::Array(vec![pick(
            items.get(n.parse::<usize>().ok()?)?,
            rest,
        )?])),
        (Segment::Index(i), Value::Array(items)) => {
            Some(Value::Array(vec![pick(items.get(*i)?, rest)?]))
        }
        (Segment::Append, Value::Array(items)) => Some(Value::Array(
            items.iter().filter_map(|i| pick(i, rest)).collect(),
        )),
        _ => None,
    }
}

fn merge(target: &mut Value, source: Value) {
    match (target, source) {
        (Value::Object(target), Value::Object(source)) => {
            for (key, value) in source {
                match target.get_mut(&key) {
                    Some(existing) => merge(existing, value),
                    None => {
                        target.insert(key, value);
                    }
                }
            }
        }
        (Value::Array(target), Value::Array(source)) => {
            for (i, value) in source.into_iter().enumerate() {
                match target.get_mut(i) {
                    Some(existing) => merge(existing, value),
                    None => target.push(value),
                }
            }
        }
        (target, source) => *target = source,
    }
}

/// Keeps only the named fields, in the response's own nesting. A list keeps its envelope and
/// narrows each item.
pub fn project(value: &Value, fields: &[Field]) -> Value {
    if let Some(items) = list_items(value) {
        let mut page = value.as_object().cloned().unwrap_or_default();
        page.insert(
            "items".into(),
            Value::Array(items.iter().map(|item| project(item, fields)).collect()),
        );
        return Value::Object(page);
    }
    let mut out = Value::Object(Map::new());
    for field in fields {
        if let Some(picked) = pick(value, &field.segments) {
            merge(&mut out, picked);
        }
    }
    out
}

pub fn list_items(value: &Value) -> Option<&Vec<Value>> {
    if value.get("object").and_then(Value::as_str) == Some("list") {
        value.get("items").and_then(Value::as_array)
    } else {
        None
    }
}

/// Columns for objects with no manifest defaults, such as a raw request's: the same rule the
/// generator uses.
pub fn guess_columns(sample: &Value) -> Vec<Field> {
    let Some(map) = sample.as_object() else {
        return Vec::new();
    };
    let first = |names: &[&str]| {
        names
            .iter()
            .find(|n| map.contains_key(**n))
            .map(|n| (*n).to_owned())
    };
    let picks: Vec<String> = [
        first(&["id"]),
        first(&[
            "name",
            "display_name",
            "number",
            "code",
            "type",
            "description",
        ]),
        first(&["customer"]),
        first(&["amount", "total"]),
        first(&["status", "active"]),
        first(&["created_at"]),
    ]
    .into_iter()
    .flatten()
    .collect();
    if picks.is_empty() {
        return fields_from(&map.keys().take(6).cloned().collect::<Vec<_>>());
    }
    fields_from(&picks)
}

fn is_money(key: &str) -> bool {
    matches!(
        key,
        "amount" | "total" | "subtotal" | "tax_total" | "balance" | "ending_balance" | "amount_off"
    ) || key.starts_with("amount_")
        || key.ends_with("_amount")
}

fn last_key(field: &Field) -> &str {
    match field.segments.last() {
        Some(Segment::Key(k) | Segment::Number(k)) => k,
        _ => "",
    }
}

/// A cell's text, and the color it's shown in.
fn cell(field: &Field, value: Option<&Value>) -> (String, Option<&'static str>) {
    let key = last_key(field);
    match value {
        None | Some(Value::Null) => ("—".into(), Some(output::DIM)),
        Some(Value::Number(n)) if is_money(key) && n.is_i64() => {
            (dollars(n.as_i64().unwrap_or(0)), None)
        }
        Some(Value::Number(n)) if key.ends_with("_at") || key == "capture_before" => {
            (n.as_i64().map(date).unwrap_or_else(|| n.to_string()), None)
        }
        Some(Value::Number(n)) if key == "percent_off" => (format!("{n}%"), None),
        Some(Value::Number(n)) if key == "size" => (bytes(n.as_u64().unwrap_or(0)), None),
        Some(Value::Number(n)) => (n.to_string(), None),
        Some(Value::Bool(b)) => (if *b { "yes" } else { "no" }.into(), None),
        Some(Value::String(s)) if key == "status" => (s.clone(), status_color(s)),
        Some(Value::String(s)) => (s.clone(), None),
        Some(Value::Object(map)) if map.values().all(Value::is_null) => {
            ("—".into(), Some(output::DIM))
        }
        Some(Value::Object(map)) => {
            let label = label(map).unwrap_or_else(|| Value::Object(map.clone()).to_string());
            (label, None)
        }
        Some(Value::Array(items)) => {
            let scalars: Vec<String> = items
                .iter()
                .filter_map(|i| match i {
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .collect();
            if scalars.len() == items.len() && items.len() <= 3 {
                (scalars.join(", "), None)
            } else {
                let n = items.len();
                (format!("{n} item{}", if n == 1 { "" } else { "s" }), None)
            }
        }
    }
}

/// An expanded object, by what a person would recognize it: `visa •••• 4242`, an email, a name.
fn label(map: &Map<String, Value>) -> Option<String> {
    let text = |k: &str| map.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    if let Some(last4) = text("last4") {
        let kind = text("brand").or(text("bank_name")).or(text("account_type"));
        return Some(match kind {
            Some(kind) => format!("{kind} •••• {last4}"),
            None => format!("•••• {last4}"),
        });
    }
    // A payment method: its card, bank account or wallet.
    if let Some(inner) = text("type")
        .and_then(|t| map.get(t))
        .and_then(Value::as_object)
        && let Some(label) = label(inner)
    {
        return Some(label);
    }
    ["display_name", "email", "name", "number", "id"]
        .iter()
        .find_map(|k| text(k))
        .map(str::to_owned)
}

fn status_color(status: &str) -> Option<&'static str> {
    match status {
        "succeeded" | "paid" | "active" | "won" | "complete" | "completed" | "trialing" => {
            Some(output::GREEN)
        }
        "failed" | "canceled" | "void" | "lost" | "uncollectible" | "past_due" | "unpaid"
        | "expired" | "blocked" => Some(output::RED),
        "pending" | "processing" | "requires_capture" | "requires_action" | "open" | "draft"
        | "needs_response" | "under_review" | "paused" | "incomplete" => Some(output::YELLOW),
        _ => None,
    }
}

pub fn dollars(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    let whole = (abs / 100).to_string();
    let mut grouped = String::new();
    for (i, c) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("{sign}${grouped}.{:02}", abs % 100)
}

fn bytes(n: u64) -> String {
    match n {
        0..1_000 => format!("{n} B"),
        1_000..1_000_000 => format!("{:.1} KB", n as f64 / 1e3),
        _ => format!("{:.1} MB", n as f64 / 1e6),
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Epoch milliseconds as a local date: `Nov 19, 4:13 PM`, with the year when it isn't this one.
pub fn date(ms: i64) -> String {
    let Some(t) = local_time(ms.div_euclid(1000)) else {
        return ms.to_string();
    };
    let this_year = local_time(crate::client::now_ms() as i64 / 1000).map(|n| n.year);
    let hour12 = match t.hour % 12 {
        0 => 12,
        h => h,
    };
    let meridiem = if t.hour < 12 { "AM" } else { "PM" };
    let month = MONTHS[t.month.clamp(1, 12) as usize - 1];
    if Some(t.year) == this_year {
        format!("{month} {}, {hour12}:{:02} {meridiem}", t.day, t.minute)
    } else {
        format!("{month} {}, {}", t.day, t.year)
    }
}

struct LocalTime {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
}

#[cfg(unix)]
fn local_time(seconds: i64) -> Option<LocalTime> {
    let time = seconds as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r writes only into `tm`, which outlives the call.
    if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
        return None;
    }
    Some(LocalTime {
        year: i64::from(tm.tm_year) + 1900,
        month: i64::from(tm.tm_mon) + 1,
        day: i64::from(tm.tm_mday),
        hour: i64::from(tm.tm_hour),
        minute: i64::from(tm.tm_min),
    })
}

/// UTC where there's no `localtime_r`.
#[cfg(not(unix))]
fn local_time(seconds: i64) -> Option<LocalTime> {
    let days = seconds.div_euclid(86_400);
    let secs = seconds.rem_euclid(86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    Some(LocalTime {
        year,
        month,
        day,
        hour: secs / 3600,
        minute: secs % 3600 / 60,
    })
}

fn terminal_width() -> Option<usize> {
    if let Some(columns) = std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()) {
        return Some(columns);
    }
    #[cfg(unix)]
    {
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: TIOCGWINSZ fills `size` for a terminal and fails harmlessly otherwise.
        if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0
            && size.ws_col > 0
        {
            return Some(usize::from(size.ws_col));
        }
    }
    None
}

fn header(field: &Field) -> String {
    let path = field.path.strip_suffix("_at").unwrap_or(&field.path);
    path.replace(['_', '.'], " ").to_uppercase()
}

fn truncate(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Prints rows under a header, keeping column widths across calls so `--all` and `--follow` can
/// stream pages.
pub struct Table {
    columns: Vec<Field>,
    widths: Option<Vec<usize>>,
    color: bool,
    fit: Option<usize>,
}

impl Table {
    pub fn new(columns: Vec<Field>) -> Self {
        let color = output::stdout_color();
        Table {
            columns,
            widths: None,
            color,
            // Only a terminal is cut to fit; a pipe or file gets whole values.
            fit: if color || std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                terminal_width()
            } else {
                None
            },
        }
    }

    pub fn rows(&mut self, items: &[Value]) {
        let out = self.render(items);
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(out.as_bytes());
        let _ = stdout.flush();
    }

    fn render(&mut self, items: &[Value]) -> String {
        let cells: Vec<Vec<(String, Option<&'static str>)>> = items
            .iter()
            .map(|item| {
                self.columns
                    .iter()
                    .map(|c| cell(c, get(item, &c.segments).as_ref()))
                    .collect()
            })
            .collect();
        let mut out = String::new();
        if self.widths.is_none() {
            let headers: Vec<String> = self.columns.iter().map(header).collect();
            let mut widths: Vec<usize> = headers
                .iter()
                .enumerate()
                .map(|(i, h)| {
                    cells
                        .iter()
                        .map(|row| row[i].0.chars().count())
                        .chain([h.chars().count()])
                        .max()
                        .unwrap_or(0)
                        .min(60)
                })
                .collect();
            if let Some(limit) = self.fit {
                // Narrow the widest column until the row fits, but no column below 8.
                while widths.iter().sum::<usize>() + 2 * widths.len().saturating_sub(1) > limit {
                    let Some((i, w)) = widths.iter().enumerate().max_by_key(|(_, w)| **w) else {
                        break;
                    };
                    if *w <= 8 {
                        break;
                    }
                    widths[i] -= 1;
                }
            }
            let line = self.line(
                &headers
                    .iter()
                    .map(|h| (h.clone(), Some(output::DIM)))
                    .collect::<Vec<_>>(),
                &widths,
            );
            out.push_str(&line);
            self.widths = Some(widths);
        }
        let widths = self.widths.clone().expect("set above");
        for row in &cells {
            out.push_str(&self.line(row, &widths));
        }
        out
    }

    fn line(&self, row: &[(String, Option<&'static str>)], widths: &[usize]) -> String {
        let mut line = String::new();
        for (i, ((text, color), width)) in row.iter().zip(widths).enumerate() {
            let text = truncate(text, *width);
            let pad = width - text.chars().count();
            let last = i + 1 == row.len();
            line.push_str(&match color {
                Some(code) => paint(code, &text, self.color),
                None => text,
            });
            if !last {
                line.push_str(&" ".repeat(pad + 2));
            }
        }
        line.push('\n');
        line
    }
}

/// Dotted paths for a nested object's set fields, so `fees` shows as `fees.total` and so on. An
/// expanded object (one with an `id`) or an object with nothing set stays one row.
/// A field's rows: a nested object becomes `parent.child` rows, except a related object (one
/// with an ID), which stays one labeled row unless it was expanded.
fn flatten(path: &str, value: &Value, expanded: &[String], open: bool, out: &mut Vec<String>) {
    let Value::Object(map) = value else {
        out.push(path.to_owned());
        return;
    };
    let open = open || expanded.iter().any(|e| e == path);
    let set: Vec<(&String, &Value)> = map
        .iter()
        .filter(|(k, v)| {
            // `live` matches the top-level object's, so nested ones only repeat it.
            !matches!(k.as_str(), "object" | "live")
                && !v.is_null()
                && v.as_object().is_none_or(|o| !o.is_empty())
        })
        .collect();
    if (map.contains_key("id") && !open) || set.is_empty() {
        out.push(path.to_owned());
        return;
    }
    for (key, child) in ordered(set) {
        flatten(&format!("{path}.{key}"), child, expanded, open, out);
    }
}

/// The ID first and `live` last, the rest as the API orders them.
fn ordered<'a>(mut fields: Vec<(&'a String, &'a Value)>) -> Vec<(&'a String, &'a Value)> {
    let rank = |k: &str| match k {
        "id" => 0,
        "live" => 2,
        _ => 1,
    };
    fields.sort_by_key(|(k, _)| rank(k));
    fields
}

/// The dotted paths an `expand` parameter names: `{"customer": true, "invoice": {"subscription": true}}`
/// gives `customer`, `invoice` and `invoice.subscription`.
pub fn expanded_paths(expand: &Value) -> Vec<String> {
    fn walk(prefix: &str, value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    if !matches!(child, Value::Bool(false)) {
                        out.push(path.clone());
                    }
                    walk(&path, child, out);
                }
            }
            Value::Array(items) => items.iter().for_each(|i| walk(prefix, i, out)),
            // A JSON-encoded query value, or a plain list.
            Value::String(s) => match serde_json::from_str::<Value>(s) {
                Ok(v @ (Value::Object(_) | Value::Array(_))) => walk(prefix, &v, out),
                _ => {
                    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                        out.push(if prefix.is_empty() {
                            part.to_owned()
                        } else {
                            format!("{prefix}.{part}")
                        });
                    }
                }
            },
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk("", expand, &mut out);
    out
}

fn object_rows(value: &Value, expanded: &[String]) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(map) = value.as_object() {
        let fields = map.iter().filter(|(k, _)| *k != "object").collect();
        for (key, child) in ordered(fields) {
            flatten(key, child, expanded, false, &mut paths);
        }
    }
    paths
}

/// One object as field and value rows: the named fields, or every top-level one.
pub fn print_object(value: &Value, fields: Option<&[Field]>, expanded: &[String]) {
    let fields: Vec<Field> = match fields {
        Some(fields) => fields.to_vec(),
        None => fields_from(&object_rows(value, expanded)),
    };
    let color = output::stdout_color();
    let width = fields
        .iter()
        .map(|f| f.path.chars().count())
        .max()
        .unwrap_or(0);
    let limit = terminal_width()
        .filter(|_| color)
        .map(|w| w.saturating_sub(width + 2).max(20));
    let mut out = String::new();
    for field in &fields {
        let (text, code) = cell(field, get(value, &field.segments).as_ref());
        let text = match limit {
            Some(limit) => truncate(&text, limit),
            None => text,
        };
        let pad = width - field.path.chars().count();
        out.push_str(&paint(output::DIM, &field.path, color));
        out.push_str(&" ".repeat(pad + 2));
        out.push_str(&match code {
            Some(code) => paint(code, &text, color),
            None => text,
        });
        out.push('\n');
    }
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(out.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fits_the_terminal() {
        let mut table = Table {
            columns: parse_fields("id,description,amount").unwrap(),
            widths: None,
            color: false,
            fit: Some(40),
        };
        let out = table.render(&[json!({"id": "pay_1234567890", "description": "A very long description of the purchase", "amount": 500})]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0].trim_end(),
            "ID              DESCRIPTION       AMOUNT"
        );
        assert_eq!(lines[1], "pay_1234567890  A very long des…  $5.00");
        assert!(lines.iter().all(|l| l.chars().count() <= 40));
    }

    #[test]
    fn formats_money() {
        assert_eq!(dollars(11995), "$119.95");
        assert_eq!(dollars(-5), "-$0.05");
        assert_eq!(dollars(123456789), "$1,234,567.89");
    }

    #[test]
    fn projects_nested_fields() {
        let payment = json!({"id": "pay_1", "amount": 500, "risk": {"level": "low", "score": 3}, "items": [{"price": "p"}]});
        let fields = parse_fields("id,risk.level,items[].price").unwrap();
        assert_eq!(
            project(&payment, &fields),
            json!({"id": "pay_1", "risk": {"level": "low"}, "items": [{"price": "p"}]})
        );
        let page = json!({"object": "list", "has_more": false, "items": [payment]});
        assert_eq!(
            project(&page, &parse_fields("id").unwrap()),
            json!({"object": "list", "has_more": false, "items": [{"id": "pay_1"}]})
        );
    }

    #[test]
    fn labels_cells() {
        let field = |p: &str| parse_fields(p).unwrap().remove(0);
        assert_eq!(cell(&field("total"), Some(&json!(250))).0, "$2.50");
        assert_eq!(
            cell(
                &field("customer"),
                Some(&json!({"id": "cus_1", "email": "a@b.co"}))
            )
            .0,
            "a@b.co"
        );
        assert_eq!(cell(&field("description"), Some(&Value::Null)).0, "—");
        assert_eq!(cell(&field("tags"), Some(&json!(["a", "b"]))).0, "a, b");
        assert_eq!(header(&field("created_at")), "CREATED");
    }

    #[test]
    fn expands_into_rows() {
        let payment = json!({
            "object": "payment", "live": false, "id": "pay_1",
            "customer": {"id": "cus_1", "email": "a@b.co", "live": false},
            "payment_method": {"id": "pm_1", "type": "card", "live": false,
                "card": {"id": "card_1", "brand": "visa", "last4": "4242", "checks": {"cvc": "pass"}}},
        });
        assert_eq!(
            object_rows(&payment, &[]),
            ["id", "customer", "payment_method", "live"]
        );
        let expanded = expanded_paths(&json!(r#"{"payment_method":true}"#));
        assert_eq!(expanded, ["payment_method"]);
        assert_eq!(
            object_rows(&payment, &expanded),
            [
                "id",
                "customer",
                "payment_method.id",
                "payment_method.type",
                "payment_method.card.id",
                "payment_method.card.brand",
                "payment_method.card.last4",
                "payment_method.card.checks.cvc",
                "live"
            ]
        );
        let field = parse_fields("payment_method").unwrap().remove(0);
        assert_eq!(
            cell(&field, payment.get("payment_method")).0,
            "visa •••• 4242"
        );
        assert_eq!(
            expanded_paths(&json!({"invoice": {"subscription": true}})),
            ["invoice", "invoice.subscription"]
        );
    }
}
