//! Answers: `{"ok":true,"result":{…}}` or `{"ok":false,"error":…}`.

use super::ErrorCode;
use crate::json::{self, Value};

/// A refusal: the fixed word, a reason to read, and what else helps the
/// caller act on it.
#[derive(Clone, Debug, PartialEq)]
pub struct Reject {
    pub code: ErrorCode,
    pub message: String,
    pub retry_after_ms: Option<u64>,
    /// Full names to choose from, for `ambiguous_pane`.
    pub candidates: Vec<String>,
}

impl Reject {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), retry_after_ms: None, candidates: Vec::new() }
    }

    pub fn retry_after(mut self, ms: u64) -> Self {
        self.retry_after_ms = Some(ms);
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Response {
    Ok(Value),
    Err(Reject),
}

impl Response {
    pub fn to_json(&self) -> String {
        let mut kv: Vec<(String, Value)> = Vec::with_capacity(5);
        match self {
            Response::Ok(result) => {
                kv.push(("ok".into(), true.into()));
                kv.push(("result".into(), result.clone()));
            }
            Response::Err(r) => {
                kv.push(("ok".into(), false.into()));
                kv.push(("error".into(), r.code.as_str().into()));
                kv.push(("message".into(), r.message.as_str().into()));
                if let Some(ms) = r.retry_after_ms {
                    kv.push(("retry_after_ms".into(), (ms as i64).into()));
                }
                if !r.candidates.is_empty() {
                    let c = r.candidates.iter().map(|s| Value::from(s.as_str())).collect();
                    kv.push(("candidates".into(), Value::Arr(c)));
                }
            }
        }
        Value::Obj(kv).to_json()
    }

    /// Read an answer line.  An `error` word this build does not know
    /// reads as `internal`, with the word kept in the message.
    pub fn parse(line: &str) -> Result<Response, String> {
        let v = json::parse(line)?;
        match v.get("ok").and_then(Value::bool) {
            Some(true) => Ok(Response::Ok(v.get("result").cloned().unwrap_or(Value::Obj(Vec::new())))),
            Some(false) => {
                let word = v.str_at("error").unwrap_or("internal");
                let mut message = v.str_at("message").unwrap_or_default().to_string();
                let code = ErrorCode::parse(word).unwrap_or_else(|| {
                    message = format!("{word}: {message}");
                    ErrorCode::Internal
                });
                Ok(Response::Err(Reject {
                    code,
                    message,
                    retry_after_ms: v.get("retry_after_ms").and_then(Value::i64).map(|n| n.max(0) as u64),
                    candidates: v
                        .get("candidates")
                        .map(|c| c.arr().iter().filter_map(Value::str).map(String::from).collect())
                        .unwrap_or_default(),
                }))
            }
            None => Err("an answer without `ok`".into()),
        }
    }
}

#[cfg(test)]
mod pane_control_tests {
    use super::*;
    use crate::json::obj;

    #[test]
    fn pane_control_answers_read_back() {
        let ok = Response::Ok(obj([("pos", 12i64.into())]));
        assert_eq!(Response::parse(&ok.to_json()).unwrap(), ok);
        let mut r = Reject::new(ErrorCode::AmbiguousPane, "two panes").retry_after(800);
        r.candidates = vec!["spg#1".into(), "spg#2".into()];
        let err = Response::Err(r);
        assert_eq!(Response::parse(&err.to_json()).unwrap(), err);
    }

    #[test]
    fn pane_control_a_later_error_word_reads_as_internal() {
        let r = Response::parse(r#"{"ok":false,"error":"brand_new","message":"m","extra":1}"#).unwrap();
        let Response::Err(r) = r else { panic!() };
        assert_eq!(r.code, ErrorCode::Internal);
        assert_eq!(r.message, "brand_new: m");
    }
}
