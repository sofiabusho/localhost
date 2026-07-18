//! Serve files and optional directory listings.

use super::errpage::site_error;
use super::map::resolve;
use super::mime;
use crate::http::{Outbound, Status};
use crate::settings::{PathRule, SiteBlock};
use std::fs;
use std::path::Path;

pub fn serve_get(site: &SiteBlock, rule: &PathRule, url_path: &str, head_only: bool) -> Outbound {
    match serve_inner(site, rule, url_path) {
        Ok(mut resp) => {
            if head_only {
                let len = resp.body.len();
                resp.body.clear();
                resp.headers
                    .retain(|(k, _)| !k.eq_ignore_ascii_case("content-length"));
                resp.headers
                    .push(("Content-Length".into(), len.to_string()));
            }
            resp
        }
        Err(code) => site_error(site, code),
    }
}

fn serve_inner(site: &SiteBlock, rule: &PathRule, url_path: &str) -> Result<Outbound, u16> {
    let path = resolve(rule, url_path).map_err(|e| e.status())?;

    let meta = match fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Status::NOT_FOUND);
        }
        Err(_) => return Err(Status::INTERNAL),
    };

    if meta.is_dir() {
        return serve_directory(site, rule, url_path, &path);
    }
    if !meta.is_file() {
        return Err(Status::FORBIDDEN);
    }

    let bytes = fs::read(&path).map_err(|_| Status::INTERNAL)?;
    let mut resp = Outbound::new(Status::OK);
    resp.headers
        .push(("Content-Type".into(), mime::from_path(&path).to_string()));
    resp.body = bytes;
    Ok(resp)
}

fn serve_directory(
    site: &SiteBlock,
    rule: &PathRule,
    url_path: &str,
    dir: &Path,
) -> Result<Outbound, u16> {
    if let Some(index_name) = &rule.index {
        let index_path = dir.join(index_name);
        if index_path.is_file() {
            let bytes = fs::read(&index_path).map_err(|_| Status::INTERNAL)?;
            let mut resp = Outbound::new(Status::OK);
            resp.headers.push((
                "Content-Type".into(),
                mime::from_path(&index_path).to_string(),
            ));
            resp.body = bytes;
            return Ok(resp);
        }
    }

    if rule.autoindex {
        let html = autoindex_html(url_path, dir).map_err(|_| Status::INTERNAL)?;
        return Ok(Outbound::html(Status::OK, html));
    }

    let _ = site;
    Err(Status::FORBIDDEN)
}

fn autoindex_html(url_path: &str, dir: &Path) -> std::io::Result<String> {
    let mut entries: Vec<(String, bool)> = Vec::new();
    for ent in fs::read_dir(dir)? {
        let ent = ent?;
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let is_dir = ent.file_type()?.is_dir();
        entries.push((name, is_dir));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let display = if url_path.ends_with('/') || url_path == "/" {
        url_path.to_string()
    } else {
        format!("{url_path}/")
    };

    let mut body = String::new();
    body.push_str("<!DOCTYPE html><html><head><meta charset=\"utf-8\">");
    body.push_str(&format!("<title>Index of {display}</title></head><body>"));
    body.push_str(&format!("<h1>Index of {display}</h1><ul>"));
    if display != "/" {
        body.push_str("<li><a href=\"../\">../</a></li>");
    }
    for (name, is_dir) in entries {
        let href = if is_dir {
            format!("{name}/")
        } else {
            name.clone()
        };
        let label = if is_dir { format!("{name}/") } else { name };
        body.push_str(&format!("<li><a href=\"{href}\">{label}</a></li>"));
    }
    body.push_str("</ul></body></html>");
    Ok(body)
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{HttpMethod, PathRule, SiteBlock};
    use std::path::PathBuf;

    #[test]
    fn serves_index_html() {
        if !PathBuf::from("www/index.html").is_file() {
            return;
        }
        let site = SiteBlock::default();
        let mut rule = PathRule::new("/".into());
        rule.methods = vec![HttpMethod::Get];
        rule.root = Some(PathBuf::from("www"));
        rule.index = Some("index.html".into());
        let out = serve_get(&site, &rule, "/", false);
        assert_eq!(out.status, 200);
        assert!(String::from_utf8_lossy(&out.body).contains("It works"));
    }

    #[test]
    fn traversal_is_forbidden() {
        let site = SiteBlock::default();
        let mut rule = PathRule::new("/".into());
        rule.methods = vec![HttpMethod::Get];
        rule.root = Some(PathBuf::from("www"));
        let out = serve_get(&site, &rule, "/../Cargo.toml", false);
        assert_eq!(out.status, 403);
    }
}
