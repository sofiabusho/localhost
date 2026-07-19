//! Run a configured CGI interpreter for a matching script extension.
//!
//! CGI execution is split so the hub drives all of it through the single
//! epoll instance instead of blocking the event loop on `poll()`/`waitpid()`:
//!   - `plan()`  resolves the script and builds the CGI environment. Pure
//!     local filesystem work (stat/canonicalize), no process or socket I/O,
//!     so it stays synchronous — same as `content::serve`'s file lookups.
//!   - `spawn()` forks + execs the interpreter and hands back non-blocking
//!     pipe fds. It does not write the body or read any output; the hub
//!     registers those fds on its own epoll instance (`src/hub.rs`) and
//!     feeds/drains them exactly like a client socket — one read or write
//!     per fd per wake.
//!   - `finish()` turns the bytes the hub collected into a response once
//!     it has observed EOF on the stdout pipe (or is giving up on a kill).

use super::errpage::site_error;
use super::map::resolve;
use crate::http::{Inbound, Outbound, Status};
use crate::settings::{PathRule, SiteBlock};
use libc::{self, c_char, c_int, pid_t};
use std::ffi::CString;
use std::net::SocketAddr;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::Duration;

/// Hard ceiling on how long a CGI job (spawn to final byte) may run. Enforced
/// by the hub on the same per-tick cadence it already uses for client
/// idle/request timeouts (see `Peer::timed_out` / `hub::reap_cgi`).
pub const CGI_TIMEOUT: Duration = Duration::from_secs(5);

/// True when this route has a CGI mapping for the request path extension.
pub fn matches_route(rule: &PathRule, url_path: &str) -> bool {
    rule.cgi_for(url_path).is_some()
}

/// Everything needed to fork + exec, fully owned so it can outlive the
/// single request-handling call and sit in the hub's job table across many
/// epoll wakes.
#[derive(Debug)]
pub struct CgiPlan {
    pub interpreter: PathBuf,
    pub script: PathBuf,
    pub env: Vec<String>,
    pub body: Vec<u8>,
    pub head_only: bool,
}

/// Resolve the script and build the environment. No process/socket I/O.
pub fn plan(
    site: &SiteBlock,
    rule: &PathRule,
    req: &Inbound,
    url_path: &str,
    listen: SocketAddr,
    head_only: bool,
) -> Result<CgiPlan, Outbound> {
    let Some(prog) = rule.cgi_for(url_path) else {
        return Err(site_error(site, Status::INTERNAL));
    };
    let interpreter = prog.bin.clone();

    let script = match resolve(rule, url_path) {
        Ok(p) => p,
        Err(e) => return Err(site_error(site, e.status())),
    };
    if !script.is_file() {
        return Err(site_error(site, Status::NOT_FOUND));
    }

    let env = build_env(&script, site, req, url_path, listen);
    Ok(CgiPlan {
        interpreter,
        script,
        env,
        body: req.body.clone(),
        head_only,
    })
}

/// A forked, exec'd CGI child. Both pipe fds are non-blocking; the caller
/// (the hub) owns registering them with epoll and driving every read/write.
pub struct SpawnedCgi {
    pub pid: pid_t,
    /// `None` when the request had no body — stdin was closed immediately
    /// so the child sees EOF without the hub needing to register anything.
    pub stdin: Option<OwnedFd>,
    pub stdout: OwnedFd,
}

/// Fork + exec. Never writes the body or reads output — see module docs.
pub fn spawn(plan: &CgiPlan) -> Result<SpawnedCgi, ()> {
    let (in_r, in_w) = make_pipe()?;
    let (out_r, out_w) = make_pipe()?;

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        close_fd(in_r);
        close_fd(in_w);
        close_fd(out_r);
        close_fd(out_w);
        return Err(());
    }

    if pid == 0 {
        // Child — never returns to Rust.
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
        enter_child(plan);
    }

    // Parent.
    close_fd(in_r);
    close_fd(out_w);

    if set_nonblock(out_r).is_err() {
        close_fd(in_w);
        close_fd(out_r);
        // Fork already happened; the child is orphaned from our point of
        // view. This branch requires fcntl() to fail on an fd we just
        // created, which in practice only happens under fd-table exhaustion
        // racing the fork — rare enough, and early enough in the child's
        // life, that a bounded blocking reap here doesn't compromise the
        // "no blocking I/O in the hot path" rule (the hot path is request
        // handling, not this one-in-a-million startup failure).
        kill_and_forget(pid);
        return Err(());
    }
    let stdout = unsafe { OwnedFd::from_raw_fd(out_r) };

    let stdin = if plan.body.is_empty() {
        close_fd(in_w); // signal EOF immediately, nothing to send
        None
    } else if set_nonblock(in_w).is_ok() {
        Some(unsafe { OwnedFd::from_raw_fd(in_w) })
    } else {
        close_fd(in_w);
        None
    };

    Ok(SpawnedCgi { pid, stdin, stdout })
}

/// Turn collected stdout bytes into a response once the hub has observed
/// EOF on the pipe.
pub fn finish(out_buf: &[u8], head_only: bool) -> Outbound {
    let mut resp = parse_cgi_stdout(out_buf);
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

fn enter_child(plan: &CgiPlan) -> ! {
    if let Some(dir) = plan.script.parent() {
        if let Ok(c) = path_c(dir) {
            unsafe {
                let _ = libc::chdir(c.as_ptr());
            }
        }
    }

    let env_c: Vec<CString> = plan
        .env
        .iter()
        .filter_map(|s| CString::new(s.as_str()).ok())
        .collect();
    let mut envp: Vec<*const c_char> = env_c.iter().map(|s| s.as_ptr()).collect();
    envp.push(ptr::null());

    let interp_c = match path_c(&plan.interpreter) {
        Ok(c) => c,
        Err(_) => unsafe { libc::_exit(127) },
    };
    let script_c = match path_c(&plan.script) {
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

    let mut env = vec![
        "GATEWAY_INTERFACE=CGI/1.1".to_string(),
        format!("SERVER_PROTOCOL={}", req.version),
        format!("REQUEST_METHOD={}", req.method),
        format!("QUERY_STRING={query}"),
        format!("CONTENT_LENGTH={}", req.body.len()),
        format!(
            "CONTENT_TYPE={}",
            req.header("content-type").unwrap_or("")
        ),
        format!("SCRIPT_FILENAME={}", script_abs.display()),
        // RFC 3875: PATH_INFO is the URL path, not a filesystem path — the
        // trailing path segments after the script name, no query string.
        // This project doesn't split a script name from extra trailing
        // segments (routes resolve the whole remaining path straight to a
        // file), so for the common case PATH_INFO mirrors SCRIPT_NAME, same
        // as most minimal CGI setups do when there's no "extra path" to
        // report.
        format!("PATH_INFO={url_path}"),
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

fn make_pipe() -> Result<(RawFd, RawFd), ()> {
    let mut fds = [0 as c_int; 2];
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if rc < 0 {
        return Err(());
    }
    Ok((fds[0], fds[1]))
}

fn set_nonblock(fd: RawFd) -> Result<(), ()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(());
    }
    Ok(())
}

fn close_fd(fd: RawFd) {
    unsafe {
        let _ = libc::close(fd);
    }
}

fn kill_and_forget(pid: pid_t) {
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        let mut status: c_int = 0;
        libc::waitpid(pid, &mut status, 0);
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
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::HttpMethod;
    use std::collections::HashMap;
    use std::fs;
    use std::os::fd::AsRawFd;

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

    /// Test-only synchronous drain: real epoll-driven feeding/draining is
    /// exercised end-to-end by `tests/integration.sh`. Here we only need to
    /// prove `plan()` + `spawn()` — the exact functions the hub calls —
    /// produce a working child, so a simple blocking poll loop is fine; this
    /// helper never runs on the server's hot path.
    fn drain_for_test(mut spawned: SpawnedCgi, body: &[u8]) -> Vec<u8> {
        if let Some(stdin) = spawned.stdin.take() {
            let fd = stdin.as_raw_fd();
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
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::WouldBlock {
                        let mut pfd = libc::pollfd {
                            fd,
                            events: libc::POLLOUT,
                            revents: 0,
                        };
                        unsafe { libc::poll(&mut pfd, 1, 1000) };
                        continue;
                    }
                    break;
                }
                off += n as usize;
            }
            drop(stdin);
        }

        let out_fd = spawned.stdout.as_raw_fd();
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe { libc::read(out_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n > 0 {
                out.extend_from_slice(&buf[..n as usize]);
                continue;
            }
            if n == 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                let mut pfd = libc::pollfd {
                    fd: out_fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                unsafe { libc::poll(&mut pfd, 1, 1000) };
                continue;
            }
            break;
        }
        let mut status: c_int = 0;
        unsafe { libc::waitpid(spawned.pid, &mut status, 0) };
        out
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

        let cgi_plan = plan(&site, &rule, &req, "/echo.py", listen, false).expect("plan");
        let spawned = spawn(&cgi_plan).expect("spawn");
        let raw = drain_for_test(spawned, &cgi_plan.body);
        let out = finish(&raw, false);

        assert_eq!(out.status, 200, "body={}", String::from_utf8_lossy(&out.body));
        let text = String::from_utf8_lossy(&out.body);
        assert!(text.contains("METHOD=POST"));
        assert!(text.contains("BODY=payload"));
        // RFC 3875: PATH_INFO is the request's URL path, not a filesystem
        // path (see the comment in build_env for why it mirrors SCRIPT_NAME
        // here rather than the absolute script path).
        assert!(text.contains("PATH_INFO=/echo.py"));

        let _ = fs::remove_dir_all(&dir);
    }
}
