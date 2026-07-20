//! Semantic checks on a parsed SiteBundle.
//!
//! Two very different kinds of "invalid" are handled here, on purpose:
//!
//! - **Fatal, whole-process** errors: a site trying to bind the same
//!   address twice, or two sites disagreeing over a shared address/port
//!   (`check_shared_binds`, `check_port_collisions`). These aren't
//!   scoped to one site — they're about how sites relate to each other on
//!   the network, and there's no sane way to "drop one side" of a port
//!   conflict, so `validate` still aborts config loading entirely for
//!   these (matches audit.md's "Configure the same port multiple times —
//!   the server should find the error").
//! - **Non-fatal, site-scoped** errors: one individual site's own schema
//!   is broken (no bind, no path blocks, a path missing `root`/`redirect`,
//!   etc. — see `check_site_schema`/`check_path`). These can't affect any
//!   other site, so audit.md's other requirement applies instead: "if one
//!   of these configurations isn't valid... your server should continue
//!   to function for the other configurations." Such a site is dropped
//!   from the bundle and reported as a warning by the caller instead of
//!   failing the whole process.

use super::schema::{PathRule, SiteBlock, SiteBundle};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

/// Validate `bundle` in place, dropping any individually-invalid site.
///
/// Returns `Ok(warnings)` — one line per dropped site — as long as at
/// least one site survives. Returns `Err` only for whole-process-fatal
/// problems: an empty bundle to begin with, a site binding the same
/// address twice, a bind/port conflict between sites, or every site
/// having been dropped as invalid (nothing left to serve).
pub fn validate(bundle: &mut SiteBundle) -> Result<Vec<String>, String> {
    if bundle.sites.is_empty() {
        return Err("no site blocks configured".into());
    }

    // Fatal: a single site binding the same address twice. This is a
    // binding mistake, not a schema mistake — same category as the
    // cross-site checks below, not the per-site ones further down.
    for (idx, site) in bundle.sites.iter().enumerate() {
        let label = format!("site#{idx}");
        let mut local_binds = HashSet::new();
        for addr in &site.binds {
            if !local_binds.insert(*addr) {
                return Err(format!("{label}: duplicate bind {addr} within the same site"));
            }
        }
    }

    // Fatal: relationships BETWEEN sites over a shared address/port.
    // Deliberately run against the full, unfiltered bundle — whether two
    // sites conflict with each other doesn't depend on whether one of
    // them will later turn out to also be individually invalid.
    check_shared_binds(bundle)?;
    check_port_collisions(bundle)?;

    // Non-fatal: each site's own schema. A site that fails here is
    // dropped — with a warning for the caller to log — instead of taking
    // every other site down with it.
    let mut warnings = Vec::new();
    let sites = std::mem::take(&mut bundle.sites);
    for (idx, site) in sites.into_iter().enumerate() {
        let label = format!("site#{idx}");
        match check_site_schema(&label, &site) {
            Ok(()) => bundle.sites.push(site),
            Err(e) => warnings.push(format!("{e} (site dropped)")),
        }
    }

    if bundle.sites.is_empty() {
        return Err("no usable site blocks remain after validation".into());
    }

    Ok(warnings)
}

/// One site's own schema: does it have what it needs to run at all,
/// independent of every other site. Never touches `bundle` — a failure
/// here can only ever affect this one site.
fn check_site_schema(label: &str, site: &SiteBlock) -> Result<(), String> {
    if site.binds.is_empty() {
        return Err(format!("{label}: at least one 'bind' is required"));
    }
    if site.paths.is_empty() {
        return Err(format!("{label}: at least one 'path' block is required"));
    }
    for path in &site.paths {
        check_path(label, path)?;
    }
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
