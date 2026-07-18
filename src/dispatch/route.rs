//! Longest-prefix `path` matching (no regular expressions).

use crate::settings::{PathRule, SiteBlock};

/// Strip query/fragment for routing.
pub fn path_only(target: &str) -> String {
    let path = target.split('?').next().unwrap_or(target);
    let path = path.split('#').next().unwrap_or(path);
    if path.is_empty() {
        "/".into()
    } else {
        path.to_string()
    }
}

pub fn match_route<'a>(site: &'a SiteBlock, path: &str) -> Option<&'a PathRule> {
    let mut best: Option<&PathRule> = None;
    let mut best_len = 0usize;
    for rule in &site.paths {
        if prefix_matches(&rule.prefix, path) && rule.prefix.len() >= best_len {
            best_len = rule.prefix.len();
            best = Some(rule);
        }
    }
    best
}

fn prefix_matches(prefix: &str, path: &str) -> bool {
    if prefix == "/" {
        return path.starts_with('/');
    }
    if path == prefix {
        return true;
    }
    let p = prefix.trim_end_matches('/');
    path.starts_with(p)
        && path.as_bytes().get(p.len()) == Some(&b'/')
}
