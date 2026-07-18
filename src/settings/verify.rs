//! Semantic checks on a parsed SiteBundle.

use super::schema::{PathRule, SiteBundle};
use std::collections::HashSet;
use std::net::SocketAddr;

pub fn validate(bundle: &SiteBundle) -> Result<(), String> {
    if bundle.sites.is_empty() {
        return Err("no site blocks configured".into());
    }

    // Audit: configuring the same port more than once is a hard error.
    // Name-based vhosts that share a port are wired in a later phase by
    // allowing multiple `name` values on one site, or by relaxing this check
    // when hostnames differ (Phase 10). For Phase 1 we fail duplicate ports.
    let mut seen_ports: HashSet<u16> = HashSet::new();
    let mut seen_binds: HashSet<SocketAddr> = HashSet::new();

    for (idx, site) in bundle.sites.iter().enumerate() {
        let label = format!("site#{idx}");
        if site.binds.is_empty() {
            return Err(format!("{label}: at least one 'bind' is required"));
        }

        for addr in &site.binds {
            if !seen_binds.insert(*addr) {
                return Err(format!(
                    "duplicate bind {addr}: already present in the configuration"
                ));
            }
            if !seen_ports.insert(addr.port()) {
                return Err(format!(
                    "duplicate port {}: bind {addr} conflicts with an earlier listen",
                    addr.port()
                ));
            }
        }

        if site.paths.is_empty() {
            return Err(format!("{label}: at least one 'path' block is required"));
        }
        for path in &site.paths {
            check_path(&label, path)?;
        }
    }
    Ok(())
}

fn check_path(label: &str, path: &PathRule) -> Result<(), String> {
    if !path.prefix.starts_with('/') {
        return Err(format!(
            "{label}: path prefix '{}' must start with '/'",
            path.prefix
        ));
    }
    if path.methods.is_empty() {
        return Err(format!(
            "{label}: path '{}' needs a non-empty methods list",
            path.prefix
        ));
    }
    if path.redirect.is_none() && path.root.is_none() {
        return Err(format!(
            "{label}: path '{}' needs 'root' or 'redirect'",
            path.prefix
        ));
    }
    if path.cgi_ext.is_some() != path.cgi_bin.is_some() {
        return Err(format!(
            "{label}: path '{}' cgi requires both extension and interpreter",
            path.prefix
        ));
    }
    Ok(())
}
