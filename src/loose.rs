//! A forgiving JSON reader for values typed on a command line: unquoted keys, single quotes,
//! trailing commas, comments, and bare words as strings (`{status: active}`).

use serde_json::{Map, Number, Value};

#[derive(Debug)]
pub struct ParseError {
    pub message: String,
    pub offset: usize,
}

impl ParseError {
    /// The message with the input and a caret under the offending character.
    pub fn render(&self, input: &str) -> String {
        let line_start = input[..self.offset.min(input.len())]
            .rfind('\n')
            .map_or(0, |i| i + 1);
        let line_end = input[line_start..]
            .find('\n')
            .map_or(input.len(), |i| line_start + i);
        let column = input[line_start..self.offset.min(input.len())]
            .chars()
            .count();
        format!(
            "{}\n  {}\n  {}^",
            self.message,
            &input[line_start..line_end],
            " ".repeat(column)
        )
    }
}

pub fn parse(input: &str) -> Result<Value, ParseError> {
    let mut parser = Parser { src: input, pos: 0 };
    parser.skip_trivia()?;
    let value = parser.value()?;
    parser.skip_trivia()?;
    if parser.pos < input.len() {
        return Err(parser.error("unexpected text after the value"));
    }
    Ok(value)
}

struct Parser<'a> {
    src: &'a str,
    pos: usize,
}

impl Parser<'_> {
    fn error(&self, message: &str) -> ParseError {
        ParseError {
            message: message.to_owned(),
            offset: self.pos,
        }
    }

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn skip_trivia(&mut self) -> Result<(), ParseError> {
        loop {
            let rest = &self.src[self.pos..];
            if let Some(c) = rest.chars().next()
                && c.is_whitespace()
            {
                self.pos += c.len_utf8();
            } else if rest.starts_with("//") {
                self.pos += rest.find('\n').unwrap_or(rest.len());
            } else if let Some(comment) = rest.strip_prefix("/*") {
                let end = comment
                    .find("*/")
                    .ok_or_else(|| self.error("unterminated comment"))?;
                self.pos += end + 4;
            } else {
                return Ok(());
            }
        }
    }

    fn value(&mut self) -> Result<Value, ParseError> {
        match self.peek() {
            Some('{') => self.object(),
            Some('[') => self.array(),
            Some('"' | '\'') => Ok(Value::String(self.string()?)),
            Some(c) if c == '-' || c == '+' || c == '.' || c.is_ascii_digit() => self.number(),
            Some(c) if is_word_char(c) => {
                let word = self.word();
                Ok(match word {
                    "true" => Value::Bool(true),
                    "false" => Value::Bool(false),
                    "null" => Value::Null,
                    _ => Value::String(word.to_owned()),
                })
            }
            Some(_) => Err(self.error("expected a value")),
            None => Err(self.error("expected a value, found the end of the input")),
        }
    }

    fn object(&mut self) -> Result<Value, ParseError> {
        self.bump();
        let mut map = Map::new();
        loop {
            self.skip_trivia()?;
            if self.peek() == Some('}') {
                self.bump();
                return Ok(Value::Object(map));
            }
            let key = match self.peek() {
                Some('"' | '\'') => self.string()?,
                Some(c) if is_word_char(c) => self.word().to_owned(),
                _ => return Err(self.error("expected a key or `}`")),
            };
            self.skip_trivia()?;
            if self.peek() != Some(':') {
                return Err(self.error("expected `:` after the key"));
            }
            self.bump();
            self.skip_trivia()?;
            let value = self.value()?;
            map.insert(key, value);
            self.skip_trivia()?;
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some('}') => {}
                _ => return Err(self.error("expected `,` or `}`")),
            }
        }
    }

    fn array(&mut self) -> Result<Value, ParseError> {
        self.bump();
        let mut items = Vec::new();
        loop {
            self.skip_trivia()?;
            if self.peek() == Some(']') {
                self.bump();
                return Ok(Value::Array(items));
            }
            items.push(self.value()?);
            self.skip_trivia()?;
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some(']') => {}
                _ => return Err(self.error("expected `,` or `]`")),
            }
        }
    }

    fn string(&mut self) -> Result<String, ParseError> {
        let quote = self.bump().expect("string starts with a quote");
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err(self.error("unterminated string")),
                Some(c) if c == quote => return Ok(out),
                Some('\\') => match self.bump() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('b') => out.push('\u{8}'),
                    Some('f') => out.push('\u{c}'),
                    Some('0') => out.push('\0'),
                    Some('u') => out.push(self.unicode_escape()?),
                    Some('\n') => {}
                    Some(c) => out.push(c),
                    None => return Err(self.error("unterminated string")),
                },
                Some(c) => out.push(c),
            }
        }
    }

    fn unicode_escape(&mut self) -> Result<char, ParseError> {
        let high = self.hex4()?;
        if (0xD800..0xDC00).contains(&high) && self.src[self.pos..].starts_with("\\u") {
            self.pos += 2;
            let low = self.hex4()?;
            let code = 0x10000 + ((high - 0xD800) << 10) + (low.wrapping_sub(0xDC00) & 0x3FF);
            return char::from_u32(code).ok_or_else(|| self.error("invalid unicode escape"));
        }
        char::from_u32(high).ok_or_else(|| self.error("invalid unicode escape"))
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let digits = self
            .src
            .get(self.pos..self.pos + 4)
            .ok_or_else(|| self.error("expected four hex digits"))?;
        let code =
            u32::from_str_radix(digits, 16).map_err(|_| self.error("expected four hex digits"))?;
        self.pos += 4;
        Ok(code)
    }

    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '+' | '.' | '_') {
                self.pos += 1;
            } else {
                break;
            }
        }
        let text = &self.src[start..self.pos];
        let normalized = text.strip_prefix('+').unwrap_or(text).replace('_', "");
        // Words that merely start with a digit or dash, such as `2fa` or `-x`, are strings.
        match serde_json::from_str::<Number>(&normalized) {
            Ok(n) => Ok(Value::Number(n)),
            Err(_) if text.chars().all(is_word_char) => Ok(Value::String(text.to_owned())),
            Err(_) => {
                self.pos = start;
                Err(self.error("invalid number"))
            }
        }
    }

    fn word(&mut self) -> &str {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if is_word_char(c) {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
        &self.src[start..self.pos]
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '$' | '-' | '.' | '@' | '/' | '+')
}

#[cfg(test)]
mod tests {
    use super::parse;
    use serde_json::json;

    #[test]
    fn reads_strict_json() {
        assert_eq!(
            parse(r#"{"a": [1, 2.5, -3e2], "b": null, "c": "x\ny"}"#).unwrap(),
            json!({"a": [1, 2.5, -300.0], "b": null, "c": "x\ny"})
        );
    }

    #[test]
    fn tolerates_unquoted_keys_and_trailing_commas() {
        assert_eq!(
            parse(r#"{name:"john", tags: ['a', 'b',], nested: {x: 2,},}"#).unwrap(),
            json!({"name": "john", "tags": ["a", "b"], "nested": {"x": 2}})
        );
    }

    #[test]
    fn reads_bare_words_as_strings() {
        assert_eq!(
            parse("{status: active, email: a@b.co, id: cus_123, flag: true}").unwrap(),
            json!({"status": "active", "email": "a@b.co", "id": "cus_123", "flag": true})
        );
        assert_eq!(parse("{code: 2fa}").unwrap(), json!({"code": "2fa"}));
    }

    #[test]
    fn skips_comments() {
        assert_eq!(
            parse("{ // the amount\n amount: 500 /* cents */ }").unwrap(),
            json!({"amount": 500})
        );
    }

    #[test]
    fn keeps_large_integers_exact() {
        assert_eq!(
            parse("{at: 1750000000000}").unwrap(),
            json!({"at": 1750000000000_i64})
        );
    }

    #[test]
    fn points_at_errors() {
        let input = "{name: \"john\" email: 1}";
        let error = parse(input).unwrap_err();
        assert_eq!(error.message, "expected `,` or `}`");
        assert!(error.render(input).ends_with("              ^"));
    }

    #[test]
    fn rejects_trailing_text() {
        assert!(parse("{a: 1} x").is_err());
    }
}
