//! POST uploads: multipart/form-data and raw bodies into `upload` dir.

use super::errpage::site_error;
use crate::http::{Inbound, Outbound, Status};
use crate::settings::{PathRule, SiteBlock};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn handle_post(site: &SiteBlock, rule: &PathRule, req: &Inbound) -> Outbound {
    let Some(upload_dir) = rule.upload_dir.as_ref() else {
        return site_error(site, Status::FORBIDDEN);
    };

    if let Err(e) = fs::create_dir_all(upload_dir) {
        eprintln!("localhost: create upload dir: {e}");
        return site_error(site, Status::INTERNAL);
    }

    let ct = req.header("content-type").unwrap_or("");
    let result = if let Some(boundary) = multipart_boundary(ct) {
        save_multipart(upload_dir, &boundary, &req.body)
    } else {
        save_raw(upload_dir, req)
    };

    match result {
        Ok(names) if names.is_empty() => site_error(site, Status::BAD_REQUEST),
        Ok(names) => {
            let body = format!("created {}\n", names.join(", "));
            Outbound::text(Status::CREATED, body).header(
                "Location",
                format!("/uploads/{}", names[0]),
            )
        }
        Err(UploadErr::BadRequest) => site_error(site, Status::BAD_REQUEST),
        Err(UploadErr::Forbidden) => site_error(site, Status::FORBIDDEN),
        Err(UploadErr::Io) => site_error(site, Status::INTERNAL),
    }
}

#[derive(Debug)]
enum UploadErr {
    BadRequest,
    Forbidden,
    Io,
}

fn multipart_boundary(content_type: &str) -> Option<String> {
    let lower = content_type.to_ascii_lowercase();
    if !lower.starts_with("multipart/form-data") {
        return None;
    }
    for part in content_type.split(';').skip(1) {
        let part = part.trim();
        let (k, v) = part.split_once('=')?;
        if k.trim().eq_ignore_ascii_case("boundary") {
            let v = v.trim().trim_matches('"');
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn save_raw(upload_dir: &Path, req: &Inbound) -> Result<Vec<String>, UploadErr> {
    let name = raw_filename(req);
    let path = join_safe(upload_dir, &name)?;
    write_file(&path, &req.body)?;
    Ok(vec![name])
}

fn raw_filename(req: &Inbound) -> String {
    if let Some(cd) = req.header("content-disposition") {
        if let Some(n) = filename_from_disposition(cd) {
            if let Some(s) = sanitize_filename(&n) {
                return s;
            }
        }
    }
    let target = req.target.split('?').next().unwrap_or("/");
    let base = target.rsplit('/').next().unwrap_or("");
    if let Some(s) = sanitize_filename(base) {
        return s;
    }
    format!("upload-{}.bin", now_stamp())
}

fn save_multipart(
    upload_dir: &Path,
    boundary: &str,
    body: &[u8],
) -> Result<Vec<String>, UploadErr> {
    let delim = format!("--{boundary}").into_bytes();
    let mut names = Vec::new();
    let mut pos = find_subslice(body, &delim, 0).ok_or(UploadErr::BadRequest)?;
    pos += delim.len();
    if body.get(pos..pos + 2) == Some(b"\r\n") {
        pos += 2;
    }

    loop {
        if body.get(pos..pos + 2) == Some(b"--") {
            break;
        }
        let next = find_subslice(body, &delim, pos).ok_or(UploadErr::BadRequest)?;
        let part = &body[pos..next];
        let part = part.strip_suffix(b"\r\n").unwrap_or(part);

        if let Some((fname, data)) = parse_part(part)? {
            if let Some(safe) = sanitize_filename(&fname) {
                let path = join_safe(upload_dir, &safe)?;
                write_file(&path, data)?;
                names.push(safe);
            }
        }

        pos = next + delim.len();
        if body.get(pos..pos + 2) == Some(b"--") {
            break;
        }
        if body.get(pos..pos + 2) == Some(b"\r\n") {
            pos += 2;
        }
    }

    Ok(names)
}

fn parse_part(part: &[u8]) -> Result<Option<(String, &[u8])>, UploadErr> {
    let split = find_subslice(part, b"\r\n\r\n", 0).ok_or(UploadErr::BadRequest)?;
    let head = std::str::from_utf8(&part[..split]).map_err(|_| UploadErr::BadRequest)?;
    let data = &part[split + 4..];

    let mut filename: Option<String> = None;
    let mut looks_like_file = false;
    for line in head.split("\r\n") {
        if line.to_ascii_lowercase().starts_with("content-disposition:") {
            if line.to_ascii_lowercase().contains("filename") {
                looks_like_file = true;
            }
            if let Some(n) = filename_from_disposition(line) {
                filename = Some(n);
                looks_like_file = true;
            }
        }
    }

    if !looks_like_file {
        return Ok(None);
    }
    let name = filename.unwrap_or_else(|| format!("part-{}.bin", now_stamp()));
    Ok(Some((name, data)))
}

fn filename_from_disposition(line: &str) -> Option<String> {
    for piece in line.split(';') {
        let piece = piece.trim();
        let lower = piece.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("filename*=") {
            let raw = &piece[piece.len() - rest.len()..];
            let raw = raw.trim().trim_matches('"');
            if let Some(idx) = raw.rfind("''") {
                return Some(raw[idx + 2..].to_string());
            }
            return Some(raw.to_string());
        }
        if let Some(rest) = lower.strip_prefix("filename=") {
            let raw = &piece[piece.len() - rest.len()..];
            return Some(raw.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn sanitize_filename(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name).trim();
    if base.is_empty() || base == "." || base == ".." {
        return None;
    }
    let mut out = String::new();
    for c in base.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+') {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() || out == "." || out == ".." {
        None
    } else {
        Some(out)
    }
}

fn join_safe(dir: &Path, name: &str) -> Result<PathBuf, UploadErr> {
    let candidate = dir.join(name);
    let dir_canon = fs::canonicalize(dir).map_err(|_| UploadErr::Io)?;
    let parent = candidate.parent().unwrap_or(dir);
    let parent_canon = fs::canonicalize(parent).map_err(|_| UploadErr::Io)?;
    if !parent_canon.starts_with(&dir_canon) {
        return Err(UploadErr::Forbidden);
    }
    Ok(candidate)
}

fn write_file(path: &Path, data: &[u8]) -> Result<(), UploadErr> {
    let mut f = File::create(path).map_err(|_| UploadErr::Io)?;
    f.write_all(data).map_err(|_| UploadErr::Io)?;
    Ok(())
}

fn find_subslice(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| from + i)
}

fn now_stamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize_filename("../x.txt").as_deref(), Some("x.txt"));
        assert_eq!(sanitize_filename("a b.txt").as_deref(), Some("a_b.txt"));
        assert!(sanitize_filename("..").is_none());
    }

    #[test]
    fn parses_boundary() {
        assert_eq!(
            multipart_boundary("multipart/form-data; boundary=----abc"),
            Some("----abc".into())
        );
    }

    #[test]
    fn multipart_extracts_file() {
        let body = b"------B\r\n\
Content-Disposition: form-data; name=\"file\"; filename=\"hi.txt\"\r\n\
Content-Type: text/plain\r\n\
\r\n\
hello\r\n\
------B\r\n\
Content-Disposition: form-data; name=\"note\"\r\n\
\r\n\
ignore\r\n\
------B--\r\n";
        let dir = std::env::temp_dir().join("localhost_upload_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let names = save_multipart(&dir, "----B", body).unwrap();
        assert_eq!(names, vec!["hi.txt".to_string()]);
        assert_eq!(fs::read(dir.join("hi.txt")).unwrap(), b"hello");
        let _ = fs::remove_dir_all(&dir);
    }
}
