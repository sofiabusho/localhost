//! Prefer configured `errpage` files; fall back to built-in HTML.

use crate::http::Outbound;
use crate::settings::SiteBlock;
use std::fs;

pub fn site_error(site: &SiteBlock, code: u16) -> Outbound {
    if let Some(path) = site.errpages.get(&code) {
        match fs::read(path) {
            Ok(bytes) => {
                let mut r = Outbound::new(code);
                r.headers
                    .push(("Content-Type".into(), "text/html; charset=utf-8".into()));
                r.body = bytes;
                return r;
            }
            Err(_) => {
                // Missing custom page → built-in.
            }
        }
    }
    Outbound::error(code)
}
