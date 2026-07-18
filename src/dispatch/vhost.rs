//! Pick the site block for a listen address + Host header.

use crate::settings::{SiteBlock, SiteBundle};
use std::net::SocketAddr;

/// First site that lists `listen` is the default when Host does not match.
pub fn select_site<'a>(
    bundle: &'a SiteBundle,
    listen: SocketAddr,
    host_header: Option<&str>,
) -> Option<&'a SiteBlock> {
    let candidates: Vec<&SiteBlock> = bundle
        .sites
        .iter()
        .filter(|s| s.binds.contains(&listen))
        .collect();
    if candidates.is_empty() {
        return None;
    }

    if let Some(host) = host_header {
        let name = normalize_host(host);
        if let Some(hit) = candidates
            .iter()
            .find(|s| s.hostnames.iter().any(|h| h.eq_ignore_ascii_case(&name)))
        {
            return Some(*hit);
        }
    }

    Some(candidates[0])
}

fn normalize_host(raw: &str) -> String {
    let trimmed = raw.trim();
    // Strip optional port; keep IPv6 bracket form intact enough for equality checks.
    if let Some(rest) = trimmed.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            return format!("[{}]", &rest[..end]);
        }
    }
    match trimmed.rfind(':') {
        Some(i) if trimmed[i + 1..].bytes().all(|b| b.is_ascii_digit()) => {
            trimmed[..i].to_string()
        }
        _ => trimmed.to_string(),
    }
}
