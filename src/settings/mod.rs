//! Configuration loading: schema, parse, and validation.

mod schema;
mod load;
mod verify;

#[allow(unused_imports)]
pub use schema::{
    BodyLimit, CgiProg, ErrPageMap, HttpMethod, PathRule, RedirectRule, SiteBlock, SiteBundle,
};
#[allow(unused_imports)]
#[allow(unused_imports)]
pub use load::parse_file;
#[allow(unused_imports)]
#[allow(unused_imports)]
pub use verify::validate;

use std::path::Path;

/// Load and validate a configuration file.
pub fn load(path: &Path) -> Result<SiteBundle, String> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        format!("cannot read config '{}': {e}", path.display())
    })?;
    let bundle = load::parse_source(&text)?;
    verify::validate(&bundle)?;
    Ok(bundle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(src: &str) -> SiteBundle {
        let bundle = load::parse_source(src).expect("parse");
        verify::validate(&bundle).expect("validate");
        bundle
    }

    #[test]
    fn parses_single_site_multi_bind() {
        let src = r#"
site {
    bind 127.0.0.1:8080;
    bind 127.0.0.1:8081;
    name app.local;
    max_body 2M;
    errpage 404 www/err/404.html;
    path / {
        methods GET POST;
        root www;
        index index.html;
        autoindex off;
    }
}
"#;
        let b = parse_ok(src);
        assert_eq!(b.sites.len(), 1);
        assert_eq!(b.sites[0].binds.len(), 2);
        assert_eq!(b.sites[0].hostnames, vec!["app.local".to_string()]);
        assert_eq!(b.sites[0].max_body.bytes(), 2 * 1024 * 1024);
        assert_eq!(b.sites[0].paths.len(), 1);
    }

    #[test]
    fn body_size_suffixes() {
        assert_eq!(BodyLimit::parse("100").unwrap().bytes(), 100);
        assert_eq!(BodyLimit::parse("10k").unwrap().bytes(), 10 * 1024);
        assert_eq!(BodyLimit::parse("1M").unwrap().bytes(), 1024 * 1024);
        assert_eq!(BodyLimit::parse("1g").unwrap().bytes(), 1024 * 1024 * 1024);
        assert!(BodyLimit::parse("").is_err());
        assert!(BodyLimit::parse("12x").is_err());
    }

    #[test]
    fn rejects_conflicting_addresses_on_same_port() {
        let src = r#"
site {
    bind 127.0.0.1:9000;
    name a;
    path / { methods GET; root www; }
}
site {
    bind 0.0.0.0:9000;
    name b;
    path / { methods GET; root www; }
}
"#;
        let bundle = load::parse_source(src).expect("parse");
        let err = verify::validate(&bundle).expect_err("dup ports");
        assert!(err.contains("9000") || err.to_lowercase().contains("port"));
    }

    #[test]
    fn allows_shared_bind_with_distinct_names() {
        let src = r#"
site {
    bind 127.0.0.1:8080;
    name one.local;
    path / { methods GET; root www; }
}
site {
    bind 127.0.0.1:8080;
    name two.local;
    path / { methods GET; root other; }
}
"#;
        let b = parse_ok(src);
        assert_eq!(b.sites.len(), 2);
        assert_eq!(b.sites[0].binds, b.sites[1].binds);
    }

    #[test]
    fn rejects_shared_bind_without_names() {
        let src = r#"
site {
    bind 127.0.0.1:8080;
    path / { methods GET; root www; }
}
site {
    bind 127.0.0.1:8080;
    name two.local;
    path / { methods GET; root other; }
}
"#;
        let bundle = load::parse_source(src).expect("parse");
        let err = verify::validate(&bundle).expect_err("needs names");
        assert!(err.to_lowercase().contains("name") || err.contains("shared"));
    }

    #[test]
    fn rejects_shared_bind_with_duplicate_hostname() {
        let src = r#"
site {
    bind 127.0.0.1:8080;
    name same.local;
    path / { methods GET; root www; }
}
site {
    bind 127.0.0.1:8080;
    name Same.Local;
    path / { methods GET; root other; }
}
"#;
        let bundle = load::parse_source(src).expect("parse");
        let err = verify::validate(&bundle).expect_err("dup hostname");
        assert!(err.to_lowercase().contains("hostname") || err.contains("duplicate"));
    }

    #[test]
    fn rejects_empty_or_missing_bind() {
        let src = r#"
site {
    name alone;
    path / { methods GET; root www; }
}
"#;
        let bundle = load::parse_source(src).expect("parse");
        assert!(verify::validate(&bundle).is_err());
    }

    #[test]
    fn rejects_unknown_directive() {
        let src = r#"
site {
    bind 127.0.0.1:1;
    nonsense yes;
    path / { methods GET; root www; }
}
"#;
        assert!(load::parse_source(src).is_err());
    }

    #[test]
    fn redirect_and_cgi_path_rules() {
        let src = r#"
site {
    bind 127.0.0.1:8080;
    max_body 512k;
    path /old {
        methods GET;
        redirect 301 /new;
    }
    path /api {
        methods GET POST;
        root www/api;
        cgi .py /usr/bin/python3;
        cgi .sh /bin/bash;
        upload www/uploads;
        autoindex on;
    }
}
"#;
        let b = parse_ok(src);
        assert_eq!(b.sites[0].paths.len(), 2);
        assert!(b.sites[0].paths[0].redirect.is_some());
        assert_eq!(b.sites[0].paths[1].cgi.len(), 2);
        assert_eq!(b.sites[0].paths[1].autoindex, true);
    }

    #[test]
    fn load_from_file_roundtrip() {
        let path = std::env::temp_dir().join("localhost_settings_test.conf");
        std::fs::write(
            &path,
            r#"site {
    bind 127.0.0.1:18080;
    path / { methods GET; root www; index index.html; }
}
"#,
        )
        .unwrap();
        let loaded = load(&path).expect("load");
        assert_eq!(loaded.sites.len(), 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn multi_port_single_hostname() {
        let src = r#"
site {
    bind 127.0.0.1:8080;
    bind 127.0.0.1:8081;
    bind 127.0.0.1:8082;
    name multi.local;
    path / { methods GET POST DELETE; root www; }
}
"#;
        let b = parse_ok(src);
        assert_eq!(b.sites[0].binds.len(), 3);
    }
}
