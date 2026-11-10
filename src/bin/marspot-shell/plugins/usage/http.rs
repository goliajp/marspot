//! One authenticated GET through `/usr/bin/curl`.
//!
//! The token goes to curl in a config read from its stdin and nowhere
//! else: an argument is visible to every process on the machine through
//! `ps`, and a file outlives the request.

use std::io::Write as _;
use std::process::{Command, Stdio};

const TIMEOUT_SECS: &str = "20";

#[derive(Debug, PartialEq)]
pub(super) struct Response {
    pub status: u16,
    /// The `retry-after` header, in seconds.
    pub retry_after: Option<i64>,
    pub body: String,
}

/// The curl config for one request.  None for a token that could not
/// sit inside a quoted config value: it came out of a file, and a line
/// break in it would be read as further curl options.
pub(super) fn curl_config(url: &str, token: &str) -> Option<String> {
    let plain = |c: char| c.is_ascii_graphic() && c != '"' && c != '\\';
    if token.is_empty() || !token.chars().all(plain) {
        return None;
    }
    Some(format!(
        "url = \"{url}\"\n\
         header = \"authorization: Bearer {token}\"\n\
         header = \"anthropic-beta: oauth-2025-04-20\"\n\
         header = \"accept: application/json\"\n"
    ))
}

/// Split what `curl -i -w '\n%{http_code}'` printed: header blocks, the
/// body, then the status on a line of its own.  A proxy puts its own
/// header block ahead of the server's; the last one is the answer's.
pub(super) fn parse_response(out: &str) -> Option<Response> {
    let (mut rest, code) = out.rsplit_once('\n')?;
    let status = code.trim().parse().ok()?;
    let mut headers = "";
    while rest.starts_with("HTTP/") {
        let Some((head, tail)) = rest.split_once("\r\n\r\n") else {
            (headers, rest) = (rest, "");
            break;
        };
        (headers, rest) = (head, tail);
    }
    let retry_after = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("retry-after").then(|| value.trim().parse().ok())?
    });
    Some(Response { status, retry_after, body: rest.to_string() })
}

/// GET `url` with the account's token.  None when curl could not be
/// run or gave no answer (no network, timeout).
pub(super) fn get(url: &str, token: &str) -> Option<Response> {
    let config = curl_config(url, token)?;
    let mut child = Command::new("/usr/bin/curl")
        // -q first: the user's ~/.curlrc is not part of this request
        .args(["-q", "-sS", "-i", "--max-time", TIMEOUT_SECS, "-w", "\\n%{http_code}", "-K", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let wrote = child.stdin.take()?.write_all(config.as_bytes());
    let out = child.wait_with_output().ok()?;
    wrote.ok()?;
    if !out.status.success() {
        return None;
    }
    parse_response(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_reaches_curl_only_as_a_quoted_header() {
        let c = curl_config("https://example.test/u", "sk-ant-oat01-Ab_c-9").unwrap();
        assert_eq!(
            c,
            "url = \"https://example.test/u\"\n\
             header = \"authorization: Bearer sk-ant-oat01-Ab_c-9\"\n\
             header = \"anthropic-beta: oauth-2025-04-20\"\n\
             header = \"accept: application/json\"\n"
        );
    }

    #[test]
    fn a_token_that_would_break_out_of_its_line_is_refused() {
        for bad in ["", "a\nurl = \"https://evil.test\"", "a\"b", "a\\b", "a b", "a\tb", "é"] {
            assert_eq!(curl_config("https://example.test/u", bad), None, "{bad:?}");
        }
    }

    #[test]
    fn an_answer_splits_into_status_headers_and_body() {
        let out = "HTTP/2 200 \r\ncontent-type: application/json\r\n\r\n{\"limits\":[]}\n200";
        assert_eq!(
            parse_response(out),
            Some(Response { status: 200, retry_after: None, body: "{\"limits\":[]}".into() })
        );
    }

    #[test]
    fn a_refusal_carries_how_long_to_wait() {
        let out = "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 120\r\ncontent-length: 2\r\n\r\n{}\n429";
        let r = parse_response(out).unwrap();
        assert_eq!((r.status, r.retry_after, r.body.as_str()), (429, Some(120), "{}"));
        let dated = "HTTP/1.1 429 Too Many Requests\r\nretry-after: Wed, 21 Oct 2026 07:28:00 GMT\r\n\r\n\n429";
        assert_eq!(parse_response(dated).unwrap().retry_after, None, "only a number of seconds is read");
    }

    #[test]
    fn a_proxys_own_header_block_is_not_the_answer() {
        let out = "HTTP/1.1 200 Connection established\r\nretry-after: 9\r\n\r\n\
                   HTTP/2 401 \r\ncontent-type: application/json\r\n\r\n{\"error\":1}\n401";
        let r = parse_response(out).unwrap();
        assert_eq!((r.status, r.retry_after, r.body.as_str()), (401, None, "{\"error\":1}"));
    }

    #[test]
    fn output_without_a_status_line_is_no_answer() {
        assert_eq!(parse_response(""), None);
        assert_eq!(parse_response("garbage"), None);
        assert_eq!(parse_response("HTTP/2 200 \r\n\r\n{}\nnot-a-code"), None);
    }
}
