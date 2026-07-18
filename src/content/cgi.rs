//! Run one configured CGI interpreter for a matching script extension.
//!
//! Child process: `execve(interpreter, [interpreter, script], env)`.
//! Request body is written to stdin and closed (EOF). Parent reads stdout
//! until the child exits or a hard timeout fires.

use super::errpage::site_error;
use super::map::resolve;
use crate::http::{Inbound, Outbound, Status};
use crate::settings::{PathRule, SiteBlock};
use libc::{self, c_char, c_int, pid_t};
use std::ffi::CString;
use std::io;
use std::net::SocketAddr;
use std::os::fd::RawFd;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::ptr;
use std::time::{Duration, Instant};

const CGI_TIMEOUT: Duration = Duration::from_secs(5);
const READ_CHUNK: usize = 8 * 1024;

/// True when this route has a CGI mapping for the request path extension.
pub fn matches_route(rule: &PathRule, url_path: &str) -> bool {
    rule.cgi_for(url_path).is_some()
}

pub fn handle(
    site: &SiteBlock,
    rule: &PathRule,
    req: &Inbound,
    url_path: &str,
    listen: SocketAddr,
    head_only: bool,
) -> Outbound {
    let Some(prog) = rule.cgi_for(url_path) else {
        return site_error(site, Status::INTERNAL);
    };
    let interpreter = &prog.bin;

    let script = match resolve(rule, url_path) {
        Ok(p) => p,
        Err(e) => return site_error(site, e.status()),
    };

    if !script.is_file() {
        return site_error(site, Status::NOT_FOUND);
    }

    match spawn_and_collect(interpreter, &script, site, req, url_path, listen) {
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
        Err(CgiFail::Timeout) => site_error(site, Status::GATEWAY_TIMEOUT),
        Err(CgiFail::Io) => site_error(site, Status::INTERNAL),
    }
}

#[derive(Debug)]
enum CgiFail {
    Timeout,
    Io,
}

fn spawn_and_collect(
    interpreter: &Path,
    script: &Path,
    site: &SiteBlock,
    req: &Inbound,
    url_path: &str,
    listen: SocketAddr,
) -> Result<Outbound, CgiFail> {
    let (in_r, in_w) = make_pipe()?;
    let (out_r, out_w) = make_pipe()?;

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        close_fd(in_r);
        close_fd(in_w);
        close_fd(out_r);
        close_fd(out_w);
        return Err(CgiFail::Io);
    }

    if pid == 0 {
        // Child — never return to Rust.
        unsafe {
            libc::close(in_w);
            libc::close(out_r);
            if libc::dup2(in_r, libc::STDIN_FILENO) < 0
                || libc::dup2(out_w, libc::STDOUT_FILENO) < 0
            {
                libc::_exit(127);
            }
            libc::close(in_r);
            libc::close(out_w);
        }
        enter_child(interpreter, script, site, req, url_path, listen);
    }

    // Parent
    close_fd(in_r);
    close_fd(out_w);

    let feed = feed_stdin(in_w, &req.body);
    close_fd(in_w);
    if feed.is_err() {
        kill_wait(pid);
        close_fd(out_r);
        return Err(CgiFail::Io);
    }

    let output = match drain_stdout(out_r, pid) {
        Ok(bytes) => bytes,
        Err(e) => {
            close_fd(out_r);
            kill_wait(pid);
            return Err(e);
        }
    };
    close_fd(out_r);

    if let Err(e) = reap(pid) {
        return Err(e);
    }

    Ok(parse_cgi_stdout(&output))
}

fn enter_child(
    interpreter: &Path,
    script: &Path,
    site: &SiteBlock,
    req: &Inbound,
    url_path: &str,
    listen: SocketAddr,
) -> ! {
    if let Some(dir) = script.parent() {
        if let Ok(c) = path_c(dir) {
            unsafe {
                let _ = libc::chdir(c.as_ptr());
            }
        }
    }

    let env = build_env(interpreter, script, site, req, url_path, listen);
    let env_c: Vec<CString> = env
        .into_iter()
        .filter_map(|s| CString::new(s).ok())
        .collect();
    let mut envp: Vec<*const c_char> = env_c.iter().map(|s| s.as_ptr()).collect();
    envp.push(ptr::null());

    let interp_c = match path_c(interpreter) {
        Ok(c) => c,
        Err(_) => unsafe { libc::_exit(127) },
    };
    let script_c = match path_c(script) {
        Ok(c) => c,
        Err(_) => unsafe { libc::_exit(127) },
    };
    let argv: [*const c_char; 3] = [interp_c.as_ptr(), script_c.as_ptr(), ptr::null()];

    unsafe {
        libc::execve(interp_c.as_ptr(), argv.as_ptr(), envp.as_ptr());
        libc::_exit(127);
    }
}

fn build_env(
    _interpreter: &Path,
    script: &Path,
    site: &SiteBlock,
    req: &Inbound,
    url_path: &str,
    listen: SocketAddr,
) -> Vec<String> {
    let query = req
        .target
        .split_once('?')
        .map(|(_, q)| q.split('#').next().unwrap_or(""))
        .unwrap_or("");

    let server_name = site
        .hostnames
        .first()
        .cloned()
        .unwrap_or_else(|| listen.ip().to_string());

    let script_abs = script
        .canonicalize()
        .unwrap_or_else(|_| script.to_path_buf());
    // Spec: CGI uses PATH_INFO as the full script path.
    let path_info = script_abs.display().to_string();

    let mut env = vec![
        format!("GATEWAY_INTERFACE=CGI/1.1"),
        format!("SERVER_PROTOCOL={}", req.version),
        format!("REQUEST_METHOD={}", req.method),
        format!("QUERY_STRING={query}"),
        format!("CONTENT_LENGTH={}", req.body.len()),
        format!(
            "CONTENT_TYPE={}",
            req.header("content-type").unwrap_or("")
        ),
        format!("SCRIPT_FILENAME={}", script_abs.display()),
        format!("PATH_INFO={path_info}"),
        format!("SCRIPT_NAME={url_path}"),
        format!("SERVER_NAME={server_name}"),
        format!("SERVER_PORT={}", listen.port()),
    ];

    if let Some(cookie) = req.header("cookie") {
        env.push(format!("HTTP_COOKIE={cookie}"));
    }
    if let Ok(path) = std::env::var("PATH") {
        env.push(format!("PATH={path}"));
    }
    env
}

fn feed_stdin(fd: RawFd, body: &[u8]) -> io::Result<()> {
    let mut off = 0usize;
    while off < body.len() {
        let n = unsafe {
            libc::write(
                fd,
                body[off..].as_ptr() as *const libc::c_void,
                body.len() - off,
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if n == 0 {
            break;
        }
        off += n as usize;
    }
    Ok(())
}

fn drain_stdout(fd: RawFd, child: pid_t) -> Result<Vec<u8>, CgiFail> {
    set_nonblock(fd)?;
    let deadline = Instant::now() + CGI_TIMEOUT;
    let mut out = Vec::new();
    let mut buf = [0u8; READ_CHUNK];
    let mut eof = false;

    while !eof {
        let remain = deadline.saturating_duration_since(Instant::now());
        if remain.is_zero() {
            return Err(CgiFail::Timeout);
        }

        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = remain.as_millis().min(i32::MAX as u128) as c_int;
        let pr = unsafe { libc::poll(&mut pfd, 1, ms) };
        if pr < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(CgiFail::Io);
        }
        if pr == 0 {
            // Timed out waiting for more output — check if child already exited.
            if child_exited(child) {
                // Drain any remaining bytes without blocking long.
                loop {
                    let n = unsafe {
                        libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                    };
                    if n <= 0 {
                        break;
                    }
                    out.extend_from_slice(&buf[..n as usize]);
                }
                break;
            }
            return Err(CgiFail::Timeout);
        }

        if pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            loop {
                let n =
                    unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if n > 0 {
                    out.extend_from_slice(&buf[..n as usize]);
                    continue;
                }
                if n == 0 {
                    eof = true;
                    break;
                }
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(CgiFail::Io);
            }
        }
    }

    Ok(out)
}

fn reap(pid: pid_t) -> Result<(), CgiFail> {
    let deadline = Instant::now() + CGI_TIMEOUT;
    let mut status: c_int = 0;
    loop {
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r == pid {
            return Ok(());
        }
        if r < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ECHILD) {
                return Ok(());
            }
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(CgiFail::Io);
        }
        if Instant::now() >= deadline {
            kill_wait(pid);
            return Err(CgiFail::Timeout);
        }
        // Brief yield without a dedicated thread sleep API dependency beyond std.
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn child_exited(pid: pid_t) -> bool {
    let mut status: c_int = 0;
    let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    r == pid || (r < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD))
}

fn kill_wait(pid: pid_t) {
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        let mut status: c_int = 0;
        let _ = libc::waitpid(pid, &mut status, 0);
    }
}

fn make_pipe() -> Result<(RawFd, RawFd), CgiFail> {
    let mut fds = [0 as c_int; 2];
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if rc < 0 {
        return Err(CgiFail::Io);
    }
    Ok((fds[0], fds[1]))
}

fn set_nonblock(fd: RawFd) -> Result<(), CgiFail> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(CgiFail::Io);
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(CgiFail::Io);
    }
    Ok(())
}

fn close_fd(fd: RawFd) {
    unsafe {
        let _ = libc::close(fd);
    }
}

fn path_c(path: &Path) -> Result<CString, ()> {
    let s = path.to_str().ok_or(())?;
    CString::new(s).map_err(|_| ())
}

/// CGI stdout: optional `Header: value` lines, blank line, then body.
fn parse_cgi_stdout(raw: &[u8]) -> Outbound {
    let (head, body) = match split_cgi_parts(raw) {
        Some(parts) => parts,
        None => {
            return Outbound::new(Status::OK)
                .header("Content-Type", "application/octet-stream")
                .with_body(raw.to_vec());
        }
    };

    let head_str = String::from_utf8_lossy(head);
    let mut status = Status::OK;
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut saw_ct = false;

    for line in head_str.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("status") {
            if let Some(code) = value.split_whitespace().next() {
                if let Ok(n) = code.parse::<u16>() {
                    status = n;
                }
            }
            continue;
        }
        if name.eq_ignore_ascii_case("content-type") {
            saw_ct = true;
        }
        // Content-Length is recomputed from the actual body we keep.
        if name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        headers.push((name.to_string(), value.to_string()));
    }

    if !saw_ct {
        headers.push(("Content-Type".into(), "text/html; charset=utf-8".into()));
    }

    let mut resp = Outbound::new(status);
    resp.headers = headers;
    resp.body = body.to_vec();
    resp
}

fn split_cgi_parts(raw: &[u8]) -> Option<(&[u8], &[u8])> {
    if let Some(i) = find_bytes(raw, b"\r\n\r\n") {
        return Some((&raw[..i], &raw[i + 4..]));
    }
    if let Some(i) = find_bytes(raw, b"\n\n") {
        return Some((&raw[..i], &raw[i + 2..]));
    }
    None
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len())
        .position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::HttpMethod;
    use std::collections::HashMap;
    use std::fs;

    #[test]
    fn extension_match_is_case_insensitive() {
        use crate::settings::CgiProg;
        let mut rule = PathRule::new("/cgi-bin".into());
        rule.cgi.push(CgiProg {
            ext: ".py".into(),
            bin: PathBuf::from("/usr/bin/python3"),
        });
        rule.cgi.push(CgiProg {
            ext: ".sh".into(),
            bin: PathBuf::from("/bin/bash"),
        });
        assert!(matches_route(&rule, "/cgi-bin/Hi.PY"));
        assert!(matches_route(&rule, "/cgi-bin/run.SH"));
        assert!(!matches_route(&rule, "/cgi-bin/hi.txt"));
    }

    #[test]
    fn parses_status_and_body() {
        let raw = b"Status: 201 Created\r\nContent-Type: text/plain\r\n\r\nhello";
        let out = parse_cgi_stdout(raw);
        assert_eq!(out.status, 201);
        assert_eq!(out.body, b"hello");
        assert!(out
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("Content-Type") && v == "text/plain"));
    }

    #[test]
    fn parses_lf_only_headers() {
        let raw = b"Content-Type: text/html\n\n<body>ok</body>";
        let out = parse_cgi_stdout(raw);
        assert_eq!(out.status, 200);
        assert_eq!(out.body, b"<body>ok</body>");
    }

    #[test]
    fn runs_python_hello_when_available() {
        if !PathBuf::from("/usr/bin/python3").is_file() {
            return;
        }
        let dir = std::env::temp_dir().join("localhost_cgi_phase9");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let script = dir.join("echo.py");
        fs::write(
            &script,
            "#!/usr/bin/env python3\n\
             import os, sys\n\
             print('Content-Type: text/plain')\n\
             print()\n\
             print('PATH_INFO=' + os.environ.get('PATH_INFO',''))\n\
             print('METHOD=' + os.environ.get('REQUEST_METHOD',''))\n\
             data = sys.stdin.read()\n\
             print('BODY=' + data)\n",
        )
        .unwrap();

        let mut site = SiteBlock::default();
        site.hostnames = vec!["cgi.test".into()];
        let mut rule = PathRule::new("/".into());
        rule.methods = vec![HttpMethod::Get, HttpMethod::Post];
        rule.root = Some(dir.clone());
        rule.cgi.push(crate::settings::CgiProg {
            ext: ".py".into(),
            bin: PathBuf::from("/usr/bin/python3"),
        });

        let mut headers = HashMap::new();
        headers.insert("content-type".into(), "text/plain".into());
        let req = Inbound {
            method: "POST".into(),
            target: "/echo.py?x=1".into(),
            version: "HTTP/1.1".into(),
            headers,
            body: b"payload".to_vec(),
        };
        let listen: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let out = handle(&site, &rule, &req, "/echo.py", listen, false);
        assert_eq!(out.status, 200, "body={}", String::from_utf8_lossy(&out.body));
        let text = String::from_utf8_lossy(&out.body);
        assert!(text.contains("METHOD=POST"));
        assert!(text.contains("BODY=payload"));
        assert!(text.contains("PATH_INFO="));
        // PATH_INFO must be the absolute script path.
        assert!(text.contains(script.canonicalize().unwrap().display().to_string().as_str()));

        let _ = fs::remove_dir_all(&dir);
    }
}
