//! Virtual-host pick and longest-prefix route matching.

mod vhost;
mod route;

pub use route::{match_route, path_only};
pub use vhost::select_site;

use crate::http::{Inbound, Outbound, Status};
use crate::settings::{HttpMethod, SiteBundle};
use std::net::SocketAddr;

/// Resolve Host + URI against the config and build a Phase-5 response.
pub fn answer(listen: SocketAddr, req: &Inbound, bundle: &SiteBundle) -> Outbound {
    let Some(site) = select_site(bundle, listen, req.header("host")) else {
        return Outbound::error(Status::INTERNAL);
    };

    let path = path_only(&req.target);
    match match_route(site, &path) {
        None => Outbound::error(Status::NOT_FOUND),
        Some(rule) => {
            let method = HttpMethod::parse(&req.method);
            if !rule.methods.iter().any(|m| *m == method) {
                let allow = rule
                    .methods
                    .iter()
                    .map(|m| m.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Outbound::error(Status::METHOD_NOT_ALLOWED).header("Allow", allow);
            }

            if let Some(redir) = &rule.redirect {
                return Outbound::new(redir.status)
                    .header("Location", redir.target.clone())
                    .header("Content-Length", "0");
            }

            // Stubs until Phase 6+ (static / upload / CGI).
            match method {
                HttpMethod::Get => Outbound::text(
                    Status::OK,
                    format!("GET stub path={} prefix={}\n", path, rule.prefix),
                ),
                HttpMethod::Head => {
                    let mut r = Outbound::text(
                        Status::OK,
                        format!("GET stub path={} prefix={}\n", path, rule.prefix),
                    );
                    r.body.clear();
                    r
                }
                HttpMethod::Post => Outbound::text(
                    Status::OK,
                    format!(
                        "POST stub path={} body_bytes={} prefix={}\n",
                        path,
                        req.body.len(),
                        rule.prefix
                    ),
                ),
                HttpMethod::Delete => Outbound::text(
                    Status::OK,
                    format!("DELETE stub path={} prefix={}\n", path, rule.prefix),
                ),
                _ => Outbound::error(Status::METHOD_NOT_ALLOWED),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{BodyLimit, PathRule, RedirectRule, SiteBlock};
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::path::PathBuf;

    fn site_a() -> SiteBlock {
        let mut s = SiteBlock::default();
        s.binds = vec!["127.0.0.1:8080".parse().unwrap()];
        s.hostnames = vec!["alpha.local".into()];
        s.max_body = BodyLimit::parse("1M").unwrap();
        let mut root = PathRule::new("/".into());
        root.methods = vec![HttpMethod::Get, HttpMethod::Post];
        root.root = Some(PathBuf::from("www"));
        let mut old = PathRule::new("/old".into());
        old.methods = vec![HttpMethod::Get];
        old.redirect = Some(RedirectRule {
            status: 301,
            target: "/".into(),
        });
        let mut api = PathRule::new("/api".into());
        api.methods = vec![HttpMethod::Get];
        api.root = Some(PathBuf::from("www/api"));
        s.paths = vec![root, old, api];
        s
    }

    fn site_b() -> SiteBlock {
        let mut s = SiteBlock::default();
        // Same bind — unit tests exercise Host pick without going through validate().
        s.binds = vec!["127.0.0.1:8080".parse().unwrap()];
        s.hostnames = vec!["beta.local".into()];
        let mut root = PathRule::new("/".into());
        root.methods = vec![HttpMethod::Get];
        root.root = Some(PathBuf::from("other"));
        s.paths = vec![root];
        s
    }

    fn bundle() -> SiteBundle {
        SiteBundle {
            sites: vec![site_a(), site_b()],
        }
    }

    fn req(method: &str, target: &str, host: &str) -> Inbound {
        let mut headers = HashMap::new();
        headers.insert("host".into(), host.into());
        Inbound {
            method: method.into(),
            target: target.into(),
            version: "HTTP/1.1".into(),
            headers,
            body: Vec::new(),
        }
    }

    #[test]
    fn host_selects_named_site() {
        let b = bundle();
        let listen: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let a = select_site(&b, listen, Some("alpha.local")).unwrap();
        assert_eq!(a.hostnames[0], "alpha.local");
        let beta = select_site(&b, listen, Some("beta.local")).unwrap();
        assert_eq!(beta.hostnames[0], "beta.local");
        // Unknown Host → first site for this bind (default).
        let def = select_site(&b, listen, Some("unknown.test")).unwrap();
        assert_eq!(def.hostnames[0], "alpha.local");
    }

    #[test]
    fn longest_prefix_wins() {
        let s = site_a();
        let r = match_route(&s, "/api/x").unwrap();
        assert_eq!(r.prefix, "/api");
        let r = match_route(&s, "/old").unwrap();
        assert_eq!(r.prefix, "/old");
        let r = match_route(&s, "/").unwrap();
        assert_eq!(r.prefix, "/");
    }

    #[test]
    fn method_not_allowed() {
        let b = bundle();
        let listen: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let out = answer(listen, &req("DELETE", "/", "alpha.local"), &b);
        assert_eq!(out.status, 405);
        assert!(out
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("Allow") && v.contains("GET")));
    }

    #[test]
    fn redirect_sets_location() {
        let b = bundle();
        let listen: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let out = answer(listen, &req("GET", "/old", "alpha.local"), &b);
        assert_eq!(out.status, 301);
        assert!(out
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("Location") && v == "/"));
    }

    #[test]
    fn host_header_port_stripped() {
        let b = bundle();
        let listen: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let site = select_site(&b, listen, Some("beta.local:8080")).unwrap();
        assert_eq!(site.hostnames[0], "beta.local");
    }

    #[test]
    fn path_only_strips_query() {
        assert_eq!(path_only("/old?x=1"), "/old");
        assert_eq!(path_only("/"), "/");
    }
}
