//! Virtual-host pick and longest-prefix route matching.

mod vhost;
mod route;

pub use route::{match_route, path_only};
pub use vhost::select_site;

use crate::content::{cgi_matches, handle_cgi, handle_delete, handle_post, serve_get, site_error};
use crate::http::{Inbound, Outbound, Status};
use crate::settings::{HttpMethod, SiteBundle};
use std::net::SocketAddr;

/// Resolve Host + URI against the config and build a response.
pub fn answer(listen: SocketAddr, req: &Inbound, bundle: &SiteBundle) -> Outbound {
    let Some(site) = select_site(bundle, listen, req.header("host")) else {
        return Outbound::error(Status::INTERNAL);
    };

    if req.body.len() as u64 > site.max_body.bytes() {
        return site_error(site, Status::PAYLOAD_TOO_LARGE);
    }

    let path = path_only(&req.target);
    match match_route(site, &path) {
        None => site_error(site, Status::NOT_FOUND),
        Some(rule) => {
            let method = HttpMethod::parse(&req.method);
            if !rule.methods.iter().any(|m| *m == method) {
                let allow = rule
                    .methods
                    .iter()
                    .map(|m| m.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return site_error(site, Status::METHOD_NOT_ALLOWED).header("Allow", allow);
            }

            if let Some(redir) = &rule.redirect {
                return Outbound::new(redir.status)
                    .header("Location", redir.target.clone())
                    .header("Content-Length", "0");
            }

            if cgi_matches(rule, &path) {
                let head_only = matches!(method, HttpMethod::Head);
                return handle_cgi(site, rule, req, &path, listen, head_only);
            }

            match method {
                HttpMethod::Get => serve_get(site, rule, &path, false),
                HttpMethod::Head => serve_get(site, rule, &path, true),
                HttpMethod::Post => handle_post(site, rule, req),
                HttpMethod::Delete => handle_delete(site, rule, &path),
                _ => site_error(site, Status::METHOD_NOT_ALLOWED),
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
        root.index = Some("index.html".into());
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

    #[test]
    fn cgi_extension_routes_to_handler() {
        use crate::content::cgi_matches;
        use crate::settings::PathRule;
        use std::path::PathBuf;

        let mut rule = PathRule::new("/cgi-bin".into());
        rule.methods = vec![HttpMethod::Get, HttpMethod::Post];
        rule.root = Some(PathBuf::from("www/cgi"));
        rule.cgi.push(crate::settings::CgiProg {
            ext: ".py".into(),
            bin: PathBuf::from("/usr/bin/python3"),
        });
        rule.cgi.push(crate::settings::CgiProg {
            ext: ".sh".into(),
            bin: PathBuf::from("/bin/bash"),
        });
        assert!(cgi_matches(&rule, "/cgi-bin/hello.py"));
        assert!(cgi_matches(&rule, "/cgi-bin/hello.sh"));
        assert!(!cgi_matches(&rule, "/cgi-bin/readme.txt"));

        if !PathBuf::from("www/cgi/hello.py").is_file()
            || !PathBuf::from("/usr/bin/python3").is_file()
        {
            return;
        }
        let mut site = SiteBlock::default();
        site.binds = vec!["127.0.0.1:8080".parse().unwrap()];
        site.hostnames = vec!["localhost".into()];
        site.paths = vec![rule];
        let bundle = SiteBundle { sites: vec![site] };
        let listen: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let out = answer(listen, &req("GET", "/cgi-bin/hello.py", "localhost"), &bundle);
        assert_eq!(out.status, 200);
        let body = String::from_utf8_lossy(&out.body);
        assert!(body.contains("REQUEST_METHOD=GET"));
    }

    #[test]
    fn get_serves_index_when_www_present() {
        let b = bundle();
        let listen: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        // Relies on repo www/index.html when tests run from crate root.
        if !PathBuf::from("www/index.html").is_file() {
            return;
        }
        let out = answer(listen, &req("GET", "/", "alpha.local"), &b);
        assert_eq!(out.status, 200);
        assert!(out
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("Content-Type") && v.contains("text/html")));
        let body = String::from_utf8_lossy(&out.body);
        assert!(body.contains("localhost") || body.contains("It works"));
    }
}
