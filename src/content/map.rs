//! Map a request URI onto a filesystem path under the route root.

use crate::http::Status;
use crate::settings::PathRule;
use std::path::{Component, Path, PathBuf};

#[derive(Debug)]
pub enum MapError {
    Forbidden,
    NotFound,
    Internal,
}

impl MapError {
    pub fn status(self) -> u16 {
        match self {
            Self::Forbidden => Status::FORBIDDEN,
            Self::NotFound => Status::NOT_FOUND,
            Self::Internal => Status::INTERNAL,
        }
    }
}

/// Resolve `url_path` against `rule.root`, rejecting traversal.
pub fn resolve(rule: &PathRule, url_path: &str) -> Result<PathBuf, MapError> {
    let root = rule.root.as_ref().ok_or(MapError::Internal)?;
    let rel = strip_prefix(&rule.prefix, url_path);
    let rel = match percent_decode(&rel) {
        Ok(s) => s,
        Err(_) => return Err(MapError::Forbidden),
    };

    for part in rel.split('/') {
        if part == ".." {
            return Err(MapError::Forbidden);
        }
    }

    let mut candidate = root.clone();
    for part in rel.split('/').filter(|p| !p.is_empty() && *p != ".") {
        candidate.push(part);
    }

    // If root exists, require resolved path stay under it.
    let root_canon = match std::fs::canonicalize(root) {
        Ok(p) => p,
        Err(_) => {
            // Root missing → 404 for anything under it.
            return Err(MapError::NotFound);
        }
    };

    match std::fs::canonicalize(&candidate) {
        Ok(canon) => {
            if !canon.starts_with(&root_canon) {
                return Err(MapError::Forbidden);
            }
            Ok(canon)
        }
        Err(_) => {
            // Path does not exist yet — still reject if any parent escapes via .. (already checked).
            // Return the non-canonical path for the caller to distinguish file-missing.
            // Ensure lexical containment:
            if !lexical_under(root, &candidate) {
                return Err(MapError::Forbidden);
            }
            Ok(candidate)
        }
    }
}

fn strip_prefix(prefix: &str, url_path: &str) -> String {
    if prefix == "/" {
        return url_path.trim_start_matches('/').to_string();
    }
    let p = prefix.trim_end_matches('/');
    if url_path == p || url_path == prefix {
        return String::new();
    }
    if let Some(rest) = url_path.strip_prefix(p) {
        return rest.trim_start_matches('/').to_string();
    }
    url_path.trim_start_matches('/').to_string()
}

fn lexical_under(root: &Path, candidate: &Path) -> bool {
    let mut base = PathBuf::new();
    for c in root.components() {
        match c {
            Component::ParentDir => {
                base.pop();
            }
            Component::CurDir => {}
            other => base.push(other.as_os_str()),
        }
    }
    let mut cur = PathBuf::new();
    for c in candidate.components() {
        match c {
            Component::ParentDir => {
                if !cur.pop() {
                    return false;
                }
            }
            Component::CurDir => {}
            other => cur.push(other.as_os_str()),
        }
    }
    cur.starts_with(&base)
}

fn percent_decode(input: &str) -> Result<String, ()> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let h = hex(bytes[i + 1])?;
                let l = hex(bytes[i + 2])?;
                out.push((h << 4) | l);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| ())
}

fn hex(b: u8) -> Result<u8, ()> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{HttpMethod, PathRule};
    use std::path::PathBuf;

    #[test]
    fn resolve_www_root_ok() {
        let mut rule = PathRule::new("/".into());
        rule.root = Some(PathBuf::from("www"));
        rule.methods = vec![HttpMethod::Get];
        let p = resolve(&rule, "/").expect("resolve /");
        eprintln!("resolved={p:?} is_dir={}", p.is_dir());
        let idx = p.join("index.html");
        eprintln!("index={idx:?} is_file={}", idx.is_file());
        assert!(p.is_dir());
        assert!(idx.is_file());
    }

    #[test]
    fn rejects_dotdot() {
        let mut rule = PathRule::new("/".into());
        rule.root = Some(PathBuf::from("www"));
        rule.methods = vec![HttpMethod::Get];
        assert!(matches!(resolve(&rule, "/../etc/passwd"), Err(MapError::Forbidden)));
        assert!(matches!(
            resolve(&rule, "/foo/../../etc/passwd"),
            Err(MapError::Forbidden)
        ));
    }

    #[test]
    fn strips_route_prefix() {
        let mut rule = PathRule::new("/static".into());
        rule.root = Some(PathBuf::from("www"));
        rule.methods = vec![HttpMethod::Get];
        // www may or may not exist in test cwd — lexical path should end with index.html
        let p = resolve(&rule, "/static/index.html").unwrap();
        assert!(p.ends_with("index.html") || p.ends_with("www/index.html"));
    }
}
