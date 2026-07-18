//! DELETE: unlink files that resolve under the route document root.

use super::errpage::site_error;
use super::map::resolve;
use crate::http::{Outbound, Status};
use crate::settings::{PathRule, SiteBlock};
use std::fs;

pub fn handle_delete(site: &SiteBlock, rule: &PathRule, url_path: &str) -> Outbound {
    if rule.root.is_none() {
        return site_error(site, Status::FORBIDDEN);
    }

    let path = match resolve(rule, url_path) {
        Ok(p) => p,
        Err(e) => return site_error(site, e.status()),
    };

    let meta = match fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return site_error(site, Status::NOT_FOUND);
        }
        Err(_) => return site_error(site, Status::INTERNAL),
    };

    if meta.is_dir() {
        return site_error(site, Status::FORBIDDEN);
    }

    match fs::remove_file(&path) {
        Ok(()) => Outbound::new(Status::NO_CONTENT)
            .header("Content-Length", "0"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            site_error(site, Status::NOT_FOUND)
        }
        Err(_) => site_error(site, Status::INTERNAL),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::HttpMethod;
    use std::path::PathBuf;

    #[test]
    fn deletes_file_under_root() {
        let dir = std::env::temp_dir().join("localhost_delete_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("gone.txt");
        fs::write(&file, b"x").unwrap();

        let site = SiteBlock::default();
        let mut rule = PathRule::new("/".into());
        rule.methods = vec![HttpMethod::Delete];
        rule.root = Some(dir.clone());

        let out = handle_delete(&site, &rule, "/gone.txt");
        assert_eq!(out.status, 204);
        assert!(!file.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_dotdot() {
        let site = SiteBlock::default();
        let mut rule = PathRule::new("/".into());
        rule.methods = vec![HttpMethod::Delete];
        rule.root = Some(PathBuf::from("www"));
        let out = handle_delete(&site, &rule, "/../Cargo.toml");
        assert_eq!(out.status, 403);
    }
}
