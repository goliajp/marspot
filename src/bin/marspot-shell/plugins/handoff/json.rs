//! A small JSON reader for agent history records.
//!
//! The rest of this module used to be written as substring matches
//! (`"\"cwd\":\""` and so on), which is fine for one flat field on a
//! hot path and wrong for anything nested: a user message that QUOTES a
//! record matches the pattern, and escaped quotes end a value early.
//! Handing a conversation over means reading nested content blocks, so
//! it gets a parser.
//!
//! Read-only, whole-value, no streaming.  Records are one line each and
//! the largest seen in the field is ~7 MB (a message carrying base64
//! images), so a line is parsed at once and then dropped.

/// A parsed JSON value.  Objects keep their key order and duplicate
/// keys; `get` returns the first, which is what every producer here
/// writes anyway.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Value>),
    Obj(Vec<(String, Value)>),
}

/// How deep nesting may go.  Not a format limit — a bound on stack
/// use, since the parser recurses and the input is a file other
/// programs write.
const MAX_DEPTH: usize = 128;

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Self::Obj(kv) => kv.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    /// `get` along a path of keys.
    pub fn at(&self, path: &[&str]) -> Option<&Value> {
        path.iter().try_fold(self, |v, k| v.get(k))
    }
    pub fn str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn arr(&self) -> &[Value] {
        match self {
            Self::Arr(a) => a,
            _ => &[],
        }
    }
    pub fn bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn i64(&self) -> Option<i64> {
        match self {
            Self::Num(n) if n.fract() == 0.0 => Some(*n as i64),
            _ => None,
        }
    }
    /// The string at `key`, the common case.
    pub fn str_at(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::str)
    }
}

/// Parse one complete JSON value; trailing whitespace is allowed,
/// anything else after it is an error.
pub fn parse(text: &str) -> Result<Value, String> {
    let mut p = Parser { b: text.as_bytes(), at: 0 };
    let v = p.value(0)?;
    p.ws();
    if p.at != p.b.len() {
        return Err(format!("trailing bytes at {}", p.at));
    }
    Ok(v)
}

struct Parser<'a> {
    b: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn err(&self, what: &str) -> String {
        format!("{what} at byte {}", self.at)
    }

    fn ws(&mut self) {
        while self.at < self.b.len() && matches!(self.b[self.at], b' ' | b'\t' | b'\n' | b'\r') {
            self.at += 1;
        }
    }

    fn eat(&mut self, lit: &[u8]) -> bool {
        if self.b[self.at..].starts_with(lit) {
            self.at += lit.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return Err(self.err("nested too deep"));
        }
        self.ws();
        match self.b.get(self.at) {
            None => Err(self.err("unexpected end")),
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string().map(Value::Str),
            Some(b't') if self.eat(b"true") => Ok(Value::Bool(true)),
            Some(b'f') if self.eat(b"false") => Ok(Value::Bool(false)),
            Some(b'n') if self.eat(b"null") => Ok(Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.err("unexpected byte")),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, String> {
        self.at += 1;
        let mut kv = Vec::new();
        self.ws();
        if self.eat(b"}") {
            return Ok(Value::Obj(kv));
        }
        loop {
            self.ws();
            if self.b.get(self.at) != Some(&b'"') {
                return Err(self.err("expected a key"));
            }
            let k = self.string()?;
            self.ws();
            if !self.eat(b":") {
                return Err(self.err("expected ':'"));
            }
            let v = self.value(depth + 1)?;
            kv.push((k, v));
            self.ws();
            if self.eat(b",") {
                continue;
            }
            if self.eat(b"}") {
                return Ok(Value::Obj(kv));
            }
            return Err(self.err("expected ',' or '}'"));
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, String> {
        self.at += 1;
        let mut out = Vec::new();
        self.ws();
        if self.eat(b"]") {
            return Ok(Value::Arr(out));
        }
        loop {
            out.push(self.value(depth + 1)?);
            self.ws();
            if self.eat(b",") {
                continue;
            }
            if self.eat(b"]") {
                return Ok(Value::Arr(out));
            }
            return Err(self.err("expected ',' or ']'"));
        }
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.at;
        while self.at < self.b.len()
            && matches!(self.b[self.at], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
        {
            self.at += 1;
        }
        // The slice is ASCII by the loop above.
        let s = std::str::from_utf8(&self.b[start..self.at]).map_err(|_| self.err("bad number"))?;
        s.parse::<f64>().map(Value::Num).map_err(|_| self.err("bad number"))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let s = self.b.get(self.at..self.at + 4).ok_or_else(|| self.err("short \\u escape"))?;
        let s = std::str::from_utf8(s).map_err(|_| self.err("bad \\u escape"))?;
        let n = u32::from_str_radix(s, 16).map_err(|_| self.err("bad \\u escape"))?;
        self.at += 4;
        Ok(n)
    }

    fn string(&mut self) -> Result<String, String> {
        self.at += 1; // opening quote
        let mut out: Vec<u8> = Vec::new();
        loop {
            // Copy the run up to the next quote or backslash in one go:
            // message bodies are long and mostly unescaped.
            let run = self.b[self.at..]
                .iter()
                .position(|&c| c == b'"' || c == b'\\')
                .ok_or_else(|| self.err("unterminated string"))?;
            out.extend_from_slice(&self.b[self.at..self.at + run]);
            self.at += run;
            if self.b[self.at] == b'"' {
                self.at += 1;
                break;
            }
            self.at += 1; // backslash
            let c = *self.b.get(self.at).ok_or_else(|| self.err("unterminated escape"))?;
            self.at += 1;
            let ch = match c {
                b'"' => '"',
                b'\\' => '\\',
                b'/' => '/',
                b'b' => '\u{8}',
                b'f' => '\u{c}',
                b'n' => '\n',
                b'r' => '\r',
                b't' => '\t',
                b'u' => {
                    let hi = self.hex4()?;
                    let cp = if (0xD800..0xDC00).contains(&hi) && self.eat(b"\\u") {
                        let lo = self.hex4()?;
                        if (0xDC00..0xE000).contains(&lo) {
                            0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                        } else {
                            0xFFFD
                        }
                    } else {
                        hi
                    };
                    char::from_u32(cp).unwrap_or('\u{FFFD}')
                }
                _ => return Err(self.err("bad escape")),
            };
            let mut buf = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
        // The input was a &str and escapes decode to whole chars, so
        // this only fails on a lone surrogate half cut mid-sequence,
        // which the escape handling already replaced.
        String::from_utf8(out).map_err(|_| self.err("invalid UTF-8"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nested_record_reads_back() {
        let v = parse(r#"{"type":"user","message":{"content":[{"type":"text","text":"hi"}]},"n":3,"ok":true,"x":null}"#)
            .unwrap();
        assert_eq!(v.str_at("type"), Some("user"));
        let blocks = v.at(&["message", "content"]).unwrap().arr();
        assert_eq!(blocks[0].str_at("text"), Some("hi"));
        assert_eq!(v.get("n").and_then(Value::i64), Some(3));
        assert_eq!(v.get("ok").and_then(Value::bool), Some(true));
        assert_eq!(v.get("x"), Some(&Value::Null));
    }

    /// The reason this exists: substring matching ends a value at the
    /// first escaped quote.
    #[test]
    fn escapes_decode_including_surrogate_pairs() {
        let v = parse(r#"{"s":"a \"q\" \\ \n \u4e2d \ud83d\ude00 end"}"#).unwrap();
        assert_eq!(v.str_at("s"), Some("a \"q\" \\ \n 中 😀 end"));
    }

    #[test]
    fn non_ascii_passes_through() {
        assert_eq!(parse(r#""交接""#).unwrap(), Value::Str("交接".into()));
    }

    #[test]
    fn malformed_input_is_an_error_not_a_panic() {
        for bad in ["", "{", "{\"a\":}", "[1,]", "\"open", "{\"a\" 1}", "tru", "1 2", "\"\\x\""] {
            assert!(parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    /// Stack use is bounded whatever the file says.
    #[test]
    fn deep_nesting_is_refused() {
        let deep = "[".repeat(10_000);
        assert!(parse(&deep).is_err());
        let ok = format!("{}{}", "[".repeat(100), "]".repeat(100));
        assert!(parse(&ok).is_ok());
    }
}
