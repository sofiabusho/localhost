#![allow(dead_code)]
//! Incremental-friendly request parsing (Content-Length + chunked).

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Inbound {
    pub method: String,
    pub target: String,
    pub version: String,
    /// Lowercased header names.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Inbound {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(|s| s.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Need more bytes in the buffer.
    Incomplete,
    BadRequest(&'static str),
    PayloadTooLarge,
}

/// Try to parse one full request from `buf`.
/// On success returns `(message, bytes_consumed)`.
pub fn try_parse(buf: &[u8], max_body: u64) -> Result<(Inbound, usize), DecodeError> {
    let head_end = match find_header_end(buf) {
        Some(i) => i,
        None => {
            if buf.len() > MAX_HEAD {
                return Err(DecodeError::BadRequest("headers too large"));
            }
            return Err(DecodeError::Incomplete);
        }
    };

    let head = std::str::from_utf8(&buf[..head_end])
        .map_err(|_| DecodeError::BadRequest("headers are not utf-8"))?;

    let (method, target, version, headers) = parse_head(head)?;

    let te = headers
        .get("transfer-encoding")
        .map(|s| s.to_ascii_lowercase());
    let cl = headers.get("content-length").cloned();

    if te.as_deref() == Some("chunked") && cl.is_some() {
        return Err(DecodeError::BadRequest(
            "both Transfer-Encoding and Content-Length",
        ));
    }

    let body_start = head_end + 4; // after \r\n\r\n

    if te.as_deref() == Some("chunked") {
        let (body, body_len) = decode_chunked(&buf[body_start..], max_body)?;
        Ok((
            Inbound {
                method,
                target,
                version,
                headers,
                body,
            },
            body_start + body_len,
        ))
    } else if let Some(cl_raw) = cl {
        let len = parse_content_length(&cl_raw)?;
        if len as u64 > max_body {
            return Err(DecodeError::PayloadTooLarge);
        }
        let need = body_start + len;
        if buf.len() < need {
            return Err(DecodeError::Incomplete);
        }
        let body = buf[body_start..need].to_vec();
        Ok((
            Inbound {
                method,
                target,
                version,
                headers,
                body,
            },
            need,
        ))
    } else {
        // No body.
        Ok((
            Inbound {
                method,
                target,
                version,
                headers,
                body: Vec::new(),
            },
            body_start,
        ))
    }
}

const MAX_HEAD: usize = 64 * 1024;

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_head(
    head: &str,
) -> Result<(String, String, String, HashMap<String, String>), DecodeError> {
    let mut lines = head.split("\r\n");
    let request_line = lines
        .next()
        .ok_or(DecodeError::BadRequest("empty request"))?;
    let mut parts = request_line.split(' ');
    let method = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or(DecodeError::BadRequest("missing method"))?
        .to_string();
    let target = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or(DecodeError::BadRequest("missing target"))?
        .to_string();
    let version = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or(DecodeError::BadRequest("missing version"))?
        .to_string();
    if parts.next().is_some() {
        return Err(DecodeError::BadRequest("malformed request line"));
    }
    if method.bytes().any(|b| !b.is_ascii_alphabetic()) {
        return Err(DecodeError::BadRequest("invalid method"));
    }
    if version != "HTTP/1.0" && version != "HTTP/1.1" {
        return Err(DecodeError::BadRequest("unsupported version"));
    }

    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = split_header(line)?;
        let key = name.to_ascii_lowercase();
        // Comma-join duplicates for simplicity (Host should be unique).
        headers
            .entry(key)
            .and_modify(|v: &mut String| {
                v.push_str(", ");
                v.push_str(value.trim());
            })
            .or_insert_with(|| value.trim().to_string());
    }
    Ok((method, target, version, headers))
}

fn split_header(line: &str) -> Result<(&str, &str), DecodeError> {
    let idx = line
        .find(':')
        .ok_or(DecodeError::BadRequest("header missing colon"))?;
    let name = &line[..idx];
    let value = &line[idx + 1..];
    if name.is_empty() || name.bytes().any(|b| b.is_ascii_whitespace()) {
        return Err(DecodeError::BadRequest("invalid header name"));
    }
    Ok((name, value))
}

fn parse_content_length(raw: &str) -> Result<usize, DecodeError> {
    let s = raw.trim();
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(DecodeError::BadRequest("bad Content-Length"));
    }
    s.parse::<usize>()
        .map_err(|_| DecodeError::BadRequest("Content-Length overflow"))
}

/// Returns (body, bytes_consumed_from_body_region_including_final_chunk).
fn decode_chunked(buf: &[u8], max_body: u64) -> Result<(Vec<u8>, usize), DecodeError> {
    let mut pos = 0usize;
    let mut body = Vec::new();
    loop {
        let line_end = find_crlf(buf, pos).ok_or(DecodeError::Incomplete)?;
        let size_line = std::str::from_utf8(&buf[pos..line_end])
            .map_err(|_| DecodeError::BadRequest("chunk size not utf-8"))?;
        // Ignore chunk extensions after ';'
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| DecodeError::BadRequest("bad chunk size"))?;
        pos = line_end + 2;

        if size == 0 {
            // Trailers: read until blank line
            loop {
                let te = find_crlf(buf, pos).ok_or(DecodeError::Incomplete)?;
                if te == pos {
                    pos += 2;
                    return Ok((body, pos));
                }
                pos = te + 2;
            }
        }

        if body.len() as u64 + size as u64 > max_body {
            return Err(DecodeError::PayloadTooLarge);
        }
        if buf.len() < pos + size + 2 {
            return Err(DecodeError::Incomplete);
        }
        body.extend_from_slice(&buf[pos..pos + size]);
        pos += size;
        if &buf[pos..pos + 2] != b"\r\n" {
            return Err(DecodeError::BadRequest("chunk missing CRLF"));
        }
        pos += 2;
    }
}

fn find_crlf(buf: &[u8], from: usize) -> Option<usize> {
    buf[from..]
        .windows(2)
        .position(|w| w == b"\r\n")
        .map(|i| from + i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_get() {
        let raw = b"GET /index.html HTTP/1.1\r\nHost: example.test\r\n\r\n";
        let (msg, n) = try_parse(raw, 1024).unwrap();
        assert_eq!(n, raw.len());
        assert_eq!(msg.method, "GET");
        assert_eq!(msg.target, "/index.html");
        assert_eq!(msg.header("host"), Some("example.test"));
        assert!(msg.body.is_empty());
    }

    #[test]
    fn parses_content_length_body() {
        let raw = b"POST /up HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhelloEXTRA";
        let (msg, n) = try_parse(raw, 1024).unwrap();
        assert_eq!(msg.body, b"hello");
        assert_eq!(n, raw.len() - 5); // EXTRA not consumed
    }

    #[test]
    fn parses_chunked_body() {
        let raw = b"POST /c HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n\
4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        let (msg, n) = try_parse(raw, 1024).unwrap();
        assert_eq!(msg.body, b"Wikipedia");
        assert_eq!(n, raw.len());
    }

    #[test]
    fn incomplete_waits() {
        let raw = b"GET / HTTP/1.1\r\nHost: x\r\n";
        assert_eq!(try_parse(raw, 1024).unwrap_err(), DecodeError::Incomplete);
        let raw = b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 4\r\n\r\nab";
        assert_eq!(try_parse(raw, 1024).unwrap_err(), DecodeError::Incomplete);
    }

    #[test]
    fn rejects_bad_request_line() {
        let raw = b"GET / too many HTTP/1.1\r\nHost: x\r\n\r\n";
        assert!(matches!(
            try_parse(raw, 1024),
            Err(DecodeError::BadRequest(_))
        ));
    }

    #[test]
    fn rejects_oversize_content_length() {
        let raw = b"POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 100\r\n\r\n";
        assert_eq!(try_parse(raw, 50).unwrap_err(), DecodeError::PayloadTooLarge);
    }

    #[test]
    fn rejects_oversize_chunked() {
        let raw = b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n\
5\r\nhello\r\n0\r\n\r\n";
        assert_eq!(try_parse(raw, 4).unwrap_err(), DecodeError::PayloadTooLarge);
    }

    #[test]
    fn rejects_both_te_and_cl() {
        let raw = b"POST / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\
Content-Length: 1\r\n\r\n0\r\n\r\n";
        assert!(matches!(
            try_parse(raw, 1024),
            Err(DecodeError::BadRequest(_))
        ));
    }
}
