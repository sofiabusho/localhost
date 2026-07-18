#![allow(dead_code)] // used by later phases
//! Typed configuration values produced by the settings parser.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;

/// Entire configuration: ordered list of virtual sites.
#[derive(Debug, Clone)]
pub struct SiteBundle {
    pub sites: Vec<SiteBlock>,
}

/// One `site { ... }` block.
#[derive(Debug, Clone)]
pub struct SiteBlock {
    pub binds: Vec<SocketAddr>,
    /// Host aliases for this site (`name`). Empty means match any Host.
    pub hostnames: Vec<String>,
    pub max_body: BodyLimit,
    pub errpages: ErrPageMap,
    pub paths: Vec<PathRule>,
}

impl Default for SiteBlock {
    fn default() -> Self {
        Self {
            binds: Vec::new(),
            hostnames: Vec::new(),
            max_body: BodyLimit::default(),
            errpages: HashMap::new(),
            paths: Vec::new(),
        }
    }
}

pub type ErrPageMap = HashMap<u16, PathBuf>;

/// Client body size ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyLimit(u64);

impl Default for BodyLimit {
    fn default() -> Self {
        Self(1024 * 1024)
    }
}

impl BodyLimit {
    pub fn bytes(self) -> u64 {
        self.0
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err("empty body size".into());
        }
        let (num_part, mult) = match raw.as_bytes().last().copied() {
            Some(b @ (b'k' | b'K' | b'm' | b'M' | b'g' | b'G')) => {
                let n = &raw[..raw.len() - 1];
                let m = match b {
                    b'k' | b'K' => 1024u64,
                    b'm' | b'M' => 1024 * 1024,
                    b'g' | b'G' => 1024 * 1024 * 1024,
                    _ => unreachable!(),
                };
                (n, m)
            }
            _ => (raw, 1u64),
        };
        if num_part.is_empty() || !num_part.bytes().all(|c| c.is_ascii_digit()) {
            return Err(format!("invalid body size '{raw}'"));
        }
        let n: u64 = num_part
            .parse()
            .map_err(|_| format!("body size out of range '{raw}'"))?;
        n.checked_mul(mult)
            .map(Self)
            .ok_or_else(|| format!("body size overflow '{raw}'"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpMethod {
    Get,
    Post,
    Delete,
    Head,
    Put,
    Options,
    Other,
}

impl HttpMethod {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_uppercase().as_str() {
            "GET" => Self::Get,
            "POST" => Self::Post,
            "DELETE" => Self::Delete,
            "HEAD" => Self::Head,
            "PUT" => Self::Put,
            "OPTIONS" => Self::Options,
            _ => Self::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Delete => "DELETE",
            Self::Head => "HEAD",
            Self::Put => "PUT",
            Self::Options => "OPTIONS",
            Self::Other => "OTHER",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RedirectRule {
    pub status: u16,
    pub target: String,
}

/// One CGI interpreter mapped to a file extension (`cgi .py /usr/bin/python3`).
#[derive(Debug, Clone)]
pub struct CgiProg {
    pub ext: String,
    pub bin: PathBuf,
}

/// One `path /prefix { ... }` route.
#[derive(Debug, Clone)]
pub struct PathRule {
    pub prefix: String,
    pub methods: Vec<HttpMethod>,
    pub root: Option<PathBuf>,
    pub index: Option<String>,
    pub autoindex: bool,
    pub redirect: Option<RedirectRule>,
    pub cgi: Vec<CgiProg>,
    pub upload_dir: Option<PathBuf>,
}

impl PathRule {
    pub fn new(prefix: String) -> Self {
        Self {
            prefix,
            methods: Vec::new(),
            root: None,
            index: None,
            autoindex: false,
            redirect: None,
            cgi: Vec::new(),
            upload_dir: None,
        }
    }

    /// Interpreter for `url_path` when the extension matches a configured CGI.
    pub fn cgi_for(&self, url_path: &str) -> Option<&CgiProg> {
        let path = url_path.split('?').next().unwrap_or(url_path);
        let file_ext = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())?
            .to_ascii_lowercase();
        self.cgi.iter().find(|prog| {
            prog.ext.trim_start_matches('.').eq_ignore_ascii_case(&file_ext)
        })
    }
}
