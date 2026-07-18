#![allow(dead_code)]
//! Brace-oriented configuration parser (custom directive names).

use super::schema::{
    BodyLimit, CgiProg, HttpMethod, PathRule, RedirectRule, SiteBlock, SiteBundle,
};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

pub fn parse_file(path: &Path) -> Result<SiteBundle, String> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        format!("cannot read config '{}': {e}", path.display())
    })?;
    parse_source(&text)
}

pub fn parse_source(src: &str) -> Result<SiteBundle, String> {
    let tokens = tokenize(src)?;
    let mut cur = 0usize;
    let mut sites = Vec::new();
    while cur < tokens.len() {
        expect_ident(&tokens, &mut cur, "site")?;
        expect_sym(&tokens, &mut cur, '{')?;
        let site = parse_site(&tokens, &mut cur)?;
        sites.push(site);
    }
    if sites.is_empty() {
        return Err("config contains no site blocks".into());
    }
    Ok(SiteBundle { sites })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Ident(String),
    Str(String),
    Sym(char),
}

fn tokenize(src: &str) -> Result<Vec<Tok>, String> {
    let mut out = Vec::new();
    let bytes = src.as_bytes();
    let mut i = 0usize;
    let mut line = 1usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\n' {
            line += 1;
            i += 1;
            continue;
        }
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // line comments starting with //
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        match c {
            b'{' | b'}' | b';' => {
                out.push(Tok::Sym(c as char));
                i += 1;
            }
            b'"' => {
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\n' {
                        return Err(format!("unterminated string near line {line}"));
                    }
                    i += 1;
                }
                if i >= bytes.len() {
                    return Err(format!("unterminated string near line {line}"));
                }
                let s = std::str::from_utf8(&bytes[start..i])
                    .map_err(|_| format!("invalid utf-8 string near line {line}"))?
                    .to_string();
                out.push(Tok::Str(s));
                i += 1;
            }
            _ => {
                let start = i;
                while i < bytes.len() {
                    let b = bytes[i];
                    if b.is_ascii_whitespace()
                        || matches!(b, b'{' | b'}' | b';' | b'"')
                    {
                        break;
                    }
                    i += 1;
                }
                let word = std::str::from_utf8(&bytes[start..i])
                    .map_err(|_| format!("invalid utf-8 token near line {line}"))?
                    .to_string();
                out.push(Tok::Ident(word));
            }
        }
    }
    Ok(out)
}

fn peek<'a>(tokens: &'a [Tok], cur: usize) -> Option<&'a Tok> {
    tokens.get(cur)
}

fn bump<'a>(tokens: &'a [Tok], cur: &mut usize) -> Result<&'a Tok, String> {
    let t = tokens
        .get(*cur)
        .ok_or_else(|| "unexpected end of config".to_string())?;
    *cur += 1;
    Ok(t)
}

fn expect_ident(tokens: &[Tok], cur: &mut usize, want: &str) -> Result<(), String> {
    match bump(tokens, cur)? {
        Tok::Ident(s) if s == want => Ok(()),
        other => Err(format!("expected '{want}', found {other:?}")),
    }
}

fn expect_sym(tokens: &[Tok], cur: &mut usize, want: char) -> Result<(), String> {
    match bump(tokens, cur)? {
        Tok::Sym(c) if *c == want => Ok(()),
        other => Err(format!("expected '{want}', found {other:?}")),
    }
}

fn take_word(tokens: &[Tok], cur: &mut usize) -> Result<String, String> {
    match bump(tokens, cur)? {
        Tok::Ident(s) | Tok::Str(s) => Ok(s.clone()),
        other => Err(format!("expected word, found {other:?}")),
    }
}

fn parse_site(tokens: &[Tok], cur: &mut usize) -> Result<SiteBlock, String> {
    let mut site = SiteBlock::default();
    loop {
        match peek(tokens, *cur) {
            Some(Tok::Sym('}')) => {
                *cur += 1;
                break;
            }
            None => return Err("unclosed site block".into()),
            Some(Tok::Ident(name)) if name == "path" => {
                *cur += 1;
                let prefix = take_word(tokens, cur)?;
                expect_sym(tokens, cur, '{')?;
                let rule = parse_path(tokens, cur, prefix)?;
                site.paths.push(rule);
            }
            Some(Tok::Ident(_)) => {
                let key = take_word(tokens, cur)?;
                match key.as_str() {
                    "bind" => {
                        let addr = take_word(tokens, cur)?;
                        expect_sym(tokens, cur, ';')?;
                        let sa: SocketAddr = addr
                            .parse()
                            .map_err(|e| format!("invalid bind '{addr}': {e}"))?;
                        site.binds.push(sa);
                    }
                    "name" => {
                        let mut names = Vec::new();
                        loop {
                            match peek(tokens, *cur) {
                                Some(Tok::Sym(';')) => {
                                    *cur += 1;
                                    break;
                                }
                                Some(Tok::Ident(_)) | Some(Tok::Str(_)) => {
                                    names.push(take_word(tokens, cur)?);
                                }
                                other => {
                                    return Err(format!(
                                        "unexpected token in name: {other:?}"
                                    ));
                                }
                            }
                        }
                        if names.is_empty() {
                            return Err("name directive needs at least one hostname".into());
                        }
                        site.hostnames = names;
                    }
                    "max_body" => {
                        let raw = take_word(tokens, cur)?;
                        expect_sym(tokens, cur, ';')?;
                        site.max_body = BodyLimit::parse(&raw)?;
                    }
                    "errpage" => {
                        let code_s = take_word(tokens, cur)?;
                        let path = take_word(tokens, cur)?;
                        expect_sym(tokens, cur, ';')?;
                        let code: u16 = code_s
                            .parse()
                            .map_err(|_| format!("invalid errpage code '{code_s}'"))?;
                        site.errpages.insert(code, PathBuf::from(path));
                    }
                    other => {
                        return Err(format!("unknown site directive '{other}'"));
                    }
                }
            }
            Some(other) => return Err(format!("unexpected token in site: {other:?}")),
        }
    }
    Ok(site)
}

fn parse_path(tokens: &[Tok], cur: &mut usize, prefix: String) -> Result<PathRule, String> {
    let mut rule = PathRule::new(prefix);
    loop {
        match peek(tokens, *cur) {
            Some(Tok::Sym('}')) => {
                *cur += 1;
                break;
            }
            None => return Err("unclosed path block".into()),
            Some(_) => {
                let key = take_word(tokens, cur)?;
                match key.as_str() {
                    "methods" => {
                        let mut methods = Vec::new();
                        loop {
                            match peek(tokens, *cur) {
                                Some(Tok::Sym(';')) => {
                                    *cur += 1;
                                    break;
                                }
                                Some(Tok::Ident(_)) | Some(Tok::Str(_)) => {
                                    let m = take_word(tokens, cur)?;
                                    methods.push(HttpMethod::parse(&m));
                                }
                                other => {
                                    return Err(format!(
                                        "unexpected token in methods: {other:?}"
                                    ));
                                }
                            }
                        }
                        if methods.is_empty() {
                            return Err("methods list is empty".into());
                        }
                        rule.methods = methods;
                    }
                    "root" => {
                        rule.root = Some(PathBuf::from(take_word(tokens, cur)?));
                        expect_sym(tokens, cur, ';')?;
                    }
                    "index" => {
                        rule.index = Some(take_word(tokens, cur)?);
                        expect_sym(tokens, cur, ';')?;
                    }
                    "autoindex" => {
                        let v = take_word(tokens, cur)?;
                        expect_sym(tokens, cur, ';')?;
                        rule.autoindex = match v.as_str() {
                            "on" | "true" | "1" => true,
                            "off" | "false" | "0" => false,
                            other => {
                                return Err(format!("autoindex expects on/off, got '{other}'"));
                            }
                        };
                    }
                    "redirect" => {
                        let status_s = take_word(tokens, cur)?;
                        let target = take_word(tokens, cur)?;
                        expect_sym(tokens, cur, ';')?;
                        let status: u16 = status_s
                            .parse()
                            .map_err(|_| format!("invalid redirect status '{status_s}'"))?;
                        if status != 301 && status != 302 {
                            return Err(format!(
                                "redirect status must be 301 or 302, got {status}"
                            ));
                        }
                        rule.redirect = Some(RedirectRule { status, target });
                    }
                    "cgi" => {
                        let ext = take_word(tokens, cur)?;
                        let bin = take_word(tokens, cur)?;
                        expect_sym(tokens, cur, ';')?;
                        let ext = if ext.starts_with('.') {
                            ext
                        } else {
                            format!(".{ext}")
                        };
                        rule.cgi.push(CgiProg {
                            ext,
                            bin: PathBuf::from(bin),
                        });
                    }
                    "upload" => {
                        rule.upload_dir = Some(PathBuf::from(take_word(tokens, cur)?));
                        expect_sym(tokens, cur, ';')?;
                    }
                    other => return Err(format!("unknown path directive '{other}'")),
                }
            }
        }
    }
    Ok(rule)
}
