#![allow(dead_code)]
//! Response serialization and stock error pages.

use super::codes::{default_error_html, reason_phrase};

#[derive(Debug, Clone)]
pub struct Outbound {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Outbound {
    pub fn new(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn text(status: u16, body: impl Into<String>) -> Self {
        let body = body.into().into_bytes();
        let mut r = Self::new(status);
        r.headers
            .push(("Content-Type".into(), "text/plain; charset=utf-8".into()));
        r.body = body;
        r
    }

    pub fn html(status: u16, body: impl Into<String>) -> Self {
        let body = body.into().into_bytes();
        let mut r = Self::new(status);
        r.headers
            .push(("Content-Type".into(), "text/html; charset=utf-8".into()));
        r.body = body;
        r
    }

    pub fn error(status: u16) -> Self {
        Self::html(status, default_error_html(status))
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    pub fn with_body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let reason = reason_phrase(self.status);
        let mut out = Vec::with_capacity(128 + self.body.len());
        out.extend_from_slice(format!("HTTP/1.1 {} {}\r\n", self.status, reason).as_bytes());
        let mut has_len = false;
        let mut has_conn = false;
        for (k, v) in &self.headers {
            if k.eq_ignore_ascii_case("content-length") {
                has_len = true;
            }
            if k.eq_ignore_ascii_case("connection") {
                has_conn = true;
            }
            out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
        }
        if !has_len {
            out.extend_from_slice(format!("Content-Length: {}\r\n", self.body.len()).as_bytes());
        }
        if !has_conn {
            out.extend_from_slice(b"Connection: close\r\n");
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&self.body);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_error_page() {
        let bytes = Outbound::error(404).to_bytes();
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.starts_with("HTTP/1.1 404 Not Found\r\n"));
        assert!(s.contains("Content-Length:"));
        assert!(s.contains("404 Not Found"));
    }
}
