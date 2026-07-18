//! Semantic checks on a parsed SiteBundle.

use super::schema::{PathRule, SiteBundle};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

pub fn validate(bundle: &SiteBundle) -> Result<(), String> {
    if bundle.sites.is_empty() {
        return Err("no site blocks configured".into());
    }

    for (idx, site) in bundle.sites.iter().enumerate() {
        let label = format!("site#{idx}");
        if site.binds.is_empty() {
            return Err(format!("{label}: at least one 'bind' is required"));
        }

        let mut local_binds = HashSet::new();
        for addr in &site.binds {
            if !local_binds.insert(*addr) {
                return Err(format!("{label}: duplicate bind {addr} within the same site"));
            }
        }

        if site.paths.is_empty() {
            return Err(format!("{label}: at least one 'path' block is required"));
        }
        for path in &site.paths {
            check_path(&label, path)?;
        }
    }

    check_shared_binds(bundle)?;
    check_port_collisions(bundle)?;
    Ok(())
}

/// Same `IP:port` on multiple sites is allowed only as name-based vhosts:
/// every co-tenant needs distinct non-empty `name` values.
fn check_shared_binds(bundle: &SiteBundle) -> Result<(), String> {
    let mut by_bind: HashMap<SocketAddr, Vec<usize>> = HashMap::new();
    for (idx, site) in bundle.sites.iter().enumerate() {
        for addr in &site.binds {
            by_bind.entry(*addr).or_default().push(idx);
        }
    }

    for (addr, indices) in by_bind {
        if indices.len() < 2 {
            continue;
        }
        let mut claimed: HashSet<String> = HashSet::new();
        for idx in indices {
            let site = &bundle.sites[idx];
            if site.hostnames.is_empty() {
                return Err(format!(
                    "bind {addr}: shared by multiple sites; site#{idx} needs at least one 'name'"
                ));
            }
            for host in &site.hostnames {
                let key = host.to_ascii_lowercase();
                if !claimed.insert(key) {
                    return Err(format!(
                        "bind {addr}: duplicate hostname '{host}' across name-based vhosts"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Distinct listen addresses that share a numeric port cannot both bind
/// (e.g. `0.0.0.0:9000` and `127.0.0.1:9000`).
fn check_port_collisions(bundle: &SiteBundle) -> Result<(), String> {
    let mut port_addrs: HashMap<u16, HashSet<SocketAddr>> = HashMap::new();
    for site in &bundle.sites {
        for addr in &site.binds {
            port_addrs.entry(addr.port()).or_default().insert(*addr);
        }
    }
    for (port, addrs) in port_addrs {
        if addrs.len() > 1 {
            let list = addrs
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "duplicate port {port}: conflicting listen addresses [{list}]"
            ));
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
    if path.cgi.is_empty() {
        // ok — CGI optional
    } else {
        let mut seen = HashSet::new();
        for prog in &path.cgi {
            if prog.ext.trim_start_matches('.').is_empty() {
                return Err(format!(
                    "{label}: path '{}' has an empty cgi extension",
                    path.prefix
                ));
            }
            if prog.bin.as_os_str().is_empty() {
                return Err(format!(
                    "{label}: path '{}' cgi interpreter path is empty",
                    path.prefix
                ));
            }
            let key = prog.ext.to_ascii_lowercase();
            if !seen.insert(key) {
                return Err(format!(
                    "{label}: path '{}' lists cgi extension '{}' more than once",
                    path.prefix, prog.ext
                ));
            }
        }
    }
    Ok(())
}
