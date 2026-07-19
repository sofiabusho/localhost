//! Process-wide I/O hub: one `epoll_wait` drives listeners, client peers,
//! and in-flight CGI pipes.
//!
//! CGI jobs are first-class citizens of the same epoll instance as client
//! sockets: a job's stdin/stdout pipe fds are registered with `wait` exactly
//! like a peer fd, and `handle_cgi_stdout`/`handle_cgi_stdin` each do at
//! most one syscall per wake, same discipline as `Peer::on_readable` /
//! `on_writable`. A peer that triggers a CGI job moves to `Phase::Cgi` and
//! stays registered `Interest::read_only()` (see `peer::link::Phase`) so a
//! client giving up early is still noticed, until the job hands back a
//! response via `Peer::finish_cgi`.

use crate::content::{self, CgiPlan};
use crate::dispatch;
use crate::http::{Outbound, Status};
use crate::ingress::Listener;
use crate::multiplex::{Interest, Ready, WaitSet};
use crate::peer::{CgiHandoff, Peer, PeerAction, PeerOutcome, Timing};
use crate::session::Vault;
use crate::settings::SiteBundle;
use libc::{c_int, epoll_event, pid_t};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

const WAIT_SLICE_MS: i32 = 250;
const CGI_READ_CHUNK: usize = 8 * 1024;
const CGI_WRITE_CHUNK: usize = 8 * 1024;

/// One CGI child in flight, tracked on the same epoll instance as peers.
/// Keyed in `jobs` by its stdout fd (the canonical job id); `stdin_index`
/// maps the stdin fd (when present) back to that same id.
struct CgiJob {
    pid: pid_t,
    owner: RawFd,
    stdin_fd: Option<OwnedFd>,
    // Only ever read via its raw fd (== this job's key in `jobs`); kept as
    // an OwnedFd purely so the pipe closes when the job is dropped.
    #[allow(dead_code)]
    stdout_fd: OwnedFd,
    body: Vec<u8>,
    body_off: usize,
    out_buf: Vec<u8>,
    stdout_eof: bool,
    reaped: bool,
    /// We've decided this job is dead — owner disconnected, or it blew past
    /// `deadline`. Distinct from `kill_sent` so we send SIGKILL exactly once.
    abandoned: bool,
    kill_sent: bool,
    deadline: Instant,
    head_only: bool,
    sites: Arc<SiteBundle>,
    listen: SocketAddr,
    host: Option<String>,
    cookie: Option<String>,
}

/// Run until interrupted.
pub fn run(bundle: SiteBundle) -> Result<(), String> {
    let mut addrs = Vec::new();
    for site in &bundle.sites {
        addrs.extend(site.binds.iter().copied());
    }
    let bundle = Arc::new(bundle);
    let sessions = Rc::new(RefCell::new(Vault::new()));

    let listeners = crate::ingress::open_listeners(&addrs)?;
    let wait = WaitSet::create().map_err(|e| format!("epoll_create1 failed: {e}"))?;
    let timing = Timing::default();

    let mut listeners_by_fd: HashMap<RawFd, Listener> = HashMap::new();
    for lst in listeners {
        let fd = lst.as_raw_fd();
        wait.add(fd, Interest::read_only(), fd as u64)
            .map_err(|e| format!("epoll_ctl ADD listener {fd}: {e}"))?;
        eprintln!("localhost: listening on {}", lst.addr());
        listeners_by_fd.insert(fd, lst);
    }

    let mut peers: HashMap<RawFd, Peer> = HashMap::new();
    let mut jobs: HashMap<RawFd, CgiJob> = HashMap::new();
    let mut stdin_index: HashMap<RawFd, RawFd> = HashMap::new();
    let mut events = vec![unsafe { std::mem::zeroed::<epoll_event>() }; 64];
    eprintln!(
        "localhost: hub running ({} listener(s)); Ctrl-C to stop",
        listeners_by_fd.len()
    );

    loop {
        let ready = wait
            .wait(&mut events, WAIT_SLICE_MS)
            .map_err(|e| format!("epoll_wait failed: {e}"))?;

        let now = Instant::now();

        for ev in ready {
            if listeners_by_fd.contains_key(&ev.fd) {
                if ev.readable {
                    accept_drain(
                        &wait,
                        &listeners_by_fd,
                        &mut peers,
                        &bundle,
                        &sessions,
                        ev.fd,
                        timing,
                    );
                }
                continue;
            }

            if jobs.contains_key(&ev.fd) {
                handle_cgi_stdout(&mut jobs, ev);
                continue;
            }

            if let Some(&job_id) = stdin_index.get(&ev.fd) {
                handle_cgi_stdin(&wait, &mut jobs, &mut stdin_index, job_id, ev);
                continue;
            }

            let Some(peer) = peers.get_mut(&ev.fd) else {
                continue;
            };

            if ev.error || (ev.hangup && !ev.readable && !ev.writable) {
                drop_peer(&wait, &mut peers, &mut jobs, ev.fd);
                continue;
            }

            let mut action = PeerAction::KeepRecv;
            let mut dead = false;
            let mut cgi_start: Option<CgiHandoff> = None;

            if ev.readable {
                match peer.on_readable() {
                    PeerOutcome::Drop => dead = true,
                    PeerOutcome::Ok(a) => action = a,
                    PeerOutcome::StartCgi(h) => cgi_start = Some(h),
                }
            }

            if !dead && cgi_start.is_none() && ev.writable {
                match peer.on_writable() {
                    PeerOutcome::Drop => dead = true,
                    PeerOutcome::Ok(a) => action = a,
                    PeerOutcome::StartCgi(_) => {
                        unreachable!("on_writable never yields StartCgi")
                    }
                }
            }

            if dead || action == PeerAction::Close {
                drop_peer(&wait, &mut peers, &mut jobs, ev.fd);
                continue;
            }

            if let Some(handoff) = cgi_start {
                let owner = ev.fd;
                let cookie = handoff.cookie.clone();
                match start_cgi_job(&wait, &mut jobs, &mut stdin_index, owner, handoff, now) {
                    Ok(()) => {
                        // Stay read_only (not Interest::none()) so a client
                        // that gives up early is still noticed: plain
                        // EPOLLERR/EPOLLHUP alone doesn't fire on an
                        // ordinary close(), only a recv() returning 0 does
                        // (see Peer::on_readable_during_cgi).
                        if let Err(e) = wait.modify(owner, Interest::read_only(), owner as u64) {
                            eprintln!("localhost: epoll_ctl MOD peer {owner} (cgi wait): {e}");
                            drop_peer(&wait, &mut peers, &mut jobs, owner);
                        }
                    }
                    Err(resp) => {
                        if let Some(p) = peers.get_mut(&owner) {
                            p.finish_cgi(cookie.as_deref(), resp);
                            if let Err(e) = wait.modify(owner, Interest::write_only(), owner as u64)
                            {
                                eprintln!("localhost: epoll_ctl MOD peer {owner}: {e}");
                                drop_peer(&wait, &mut peers, &mut jobs, owner);
                            }
                        }
                    }
                }
                continue;
            }

            let interest = match action {
                PeerAction::KeepRecv => Interest::read_only(),
                PeerAction::WantSend => Interest::write_only(),
                PeerAction::Close => unreachable!(),
            };
            if let Err(e) = wait.modify(ev.fd, interest, ev.fd as u64) {
                eprintln!("localhost: epoll_ctl MOD peer {}: {e}", ev.fd);
                drop_peer(&wait, &mut peers, &mut jobs, ev.fd);
            }
        }

        sessions.borrow_mut().sweep();
        reap_timeouts(&wait, &mut peers, &mut jobs, now);
        reap_cgi(&wait, &mut jobs, &mut stdin_index, &mut peers, now);
    }
}

fn accept_drain(
    wait: &WaitSet,
    listeners: &HashMap<RawFd, Listener>,
    peers: &mut HashMap<RawFd, Peer>,
    bundle: &Arc<SiteBundle>,
    sessions: &Rc<RefCell<Vault>>,
    listen_fd: RawFd,
    timing: Timing,
) {
    let Some(listener) = listeners.get(&listen_fd) else {
        return;
    };
    let listen_addr = listener.addr();
    let max_body = max_body_for(bundle, listen_addr);

    loop {
        match listener.accept_nonblocking() {
            Ok((client, peer_addr)) => {
                let fd = client.as_raw_fd();
                if let Err(e) = wait.add(fd, Interest::read_only(), fd as u64) {
                    eprintln!("localhost: epoll_ctl ADD peer {fd}: {e}");
                    drop(client);
                    continue;
                }
                let peer = Peer::new(
                    client,
                    peer_addr,
                    listen_addr,
                    timing,
                    max_body,
                    Arc::clone(bundle),
                    Rc::clone(sessions),
                );
                peers.insert(fd, peer);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("localhost: accept error on {listen_addr}: {e}");
                break;
            }
        }
    }
}

fn max_body_for(bundle: &SiteBundle, addr: SocketAddr) -> u64 {
    let mut ceiling = 0u64;
    for site in &bundle.sites {
        if site.binds.contains(&addr) {
            ceiling = ceiling.max(site.max_body.bytes());
        }
    }
    if ceiling == 0 {
        1024 * 1024
    } else {
        ceiling
    }
}

fn drop_peer(
    wait: &WaitSet,
    peers: &mut HashMap<RawFd, Peer>,
    jobs: &mut HashMap<RawFd, CgiJob>,
    fd: RawFd,
) {
    if peers.remove(&fd).is_some() {
        let _ = wait.remove(fd);
    }
    // Any CGI job this peer started is now unwanted. Mark it abandoned and
    // let the next `reap_cgi` pass do the actual SIGKILL + WNOHANG + fd
    // teardown — funneling cleanup through one place means we never risk
    // leaving a zombie behind just because its owner disappeared first.
    for job in jobs.values_mut() {
        if job.owner == fd {
            job.abandoned = true;
        }
    }
}

fn reap_timeouts(
    wait: &WaitSet,
    peers: &mut HashMap<RawFd, Peer>,
    jobs: &mut HashMap<RawFd, CgiJob>,
    now: Instant,
) {
    let expired: Vec<RawFd> = peers
        .iter()
        .filter_map(|(fd, p)| if p.timed_out(now) { Some(*fd) } else { None })
        .collect();
    for fd in expired {
        eprintln!("localhost: peer fd {fd} timed out");
        drop_peer(wait, peers, jobs, fd);
    }
}

/// Fork the CGI child and register its pipe fds with `wait`. Returns the
/// response to send immediately on failure (fork/pipe/epoll_ctl error) —
/// matching the original synchronous CGI handler's precedent, this does not
/// consult the site's custom error pages for this specific rare failure.
fn start_cgi_job(
    wait: &WaitSet,
    jobs: &mut HashMap<RawFd, CgiJob>,
    stdin_index: &mut HashMap<RawFd, RawFd>,
    owner: RawFd,
    handoff: CgiHandoff,
    now: Instant,
) -> Result<(), Outbound> {
    let spawned = content::spawn_cgi(&handoff.plan).map_err(|_| Outbound::error(500))?;
    let job_id = spawned.stdout.as_raw_fd();

    if let Err(e) = wait.add(job_id, Interest::read_only(), job_id as u64) {
        eprintln!("localhost: epoll_ctl ADD cgi stdout {job_id}: {e}");
        return Err(Outbound::error(500));
    }
    if let Some(stdin) = &spawned.stdin {
        let sfd = stdin.as_raw_fd();
        if let Err(e) = wait.add(sfd, Interest::write_only(), sfd as u64) {
            eprintln!("localhost: epoll_ctl ADD cgi stdin {sfd}: {e}");
            let _ = wait.remove(job_id);
            return Err(Outbound::error(500));
        }
        stdin_index.insert(sfd, job_id);
    }

    let CgiHandoff {
        plan: CgiPlan {
            body, head_only, ..
        },
        sites,
        listen,
        host,
        cookie,
    } = handoff;

    jobs.insert(
        job_id,
        CgiJob {
            pid: spawned.pid,
            owner,
            stdin_fd: spawned.stdin,
            stdout_fd: spawned.stdout,
            body,
            body_off: 0,
            out_buf: Vec::new(),
            stdout_eof: false,
            reaped: false,
            abandoned: false,
            kill_sent: false,
            deadline: now + content::CGI_TIMEOUT,
            head_only,
            sites,
            listen,
            host,
            cookie,
        },
    );
    Ok(())
}

/// One `read` on a CGI stdout pipe. Mirrors `Peer::on_readable`: at most one
/// syscall, EOF/error just flags the job so `reap_cgi` can finalize it once
/// the child is also reaped (no blocking wait here).
fn handle_cgi_stdout(jobs: &mut HashMap<RawFd, CgiJob>, ev: Ready) {
    let Some(job) = jobs.get_mut(&ev.fd) else {
        return;
    };
    if job.stdout_eof {
        return;
    }
    if ev.readable {
        let mut buf = [0u8; CGI_READ_CHUNK];
        let n = unsafe {
            libc::read(
                ev.fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };
        if n > 0 {
            job.out_buf.extend_from_slice(&buf[..n as usize]);
        } else if n == 0 {
            job.stdout_eof = true;
        } else {
            let err = std::io::Error::last_os_error();
            if err.kind() != ErrorKind::WouldBlock && err.kind() != ErrorKind::Interrupted {
                job.stdout_eof = true; // treat an unexpected read error as end-of-output
            }
        }
    } else if ev.error || ev.hangup {
        job.stdout_eof = true;
    }
}

/// One `write` on a CGI stdin pipe. Mirrors `Peer::on_writable`. Once the
/// whole body has been written (or the child stops reading it — CGI scripts
/// aren't required to drain stdin), the fd is dropped and deregistered;
/// that close is what signals EOF to the child.
fn handle_cgi_stdin(
    wait: &WaitSet,
    jobs: &mut HashMap<RawFd, CgiJob>,
    stdin_index: &mut HashMap<RawFd, RawFd>,
    job_id: RawFd,
    ev: Ready,
) {
    let Some(job) = jobs.get_mut(&job_id) else {
        stdin_index.remove(&ev.fd);
        return;
    };

    let done = if ev.writable {
        let remaining = &job.body[job.body_off..];
        if remaining.is_empty() {
            true
        } else {
            let want = remaining.len().min(CGI_WRITE_CHUNK);
            let n = unsafe {
                libc::write(
                    ev.fd,
                    remaining.as_ptr() as *const libc::c_void,
                    want,
                )
            };
            if n > 0 {
                job.body_off += n as usize;
                job.body_off >= job.body.len()
            } else if n == 0 {
                true
            } else {
                let err = std::io::Error::last_os_error();
                !(err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::Interrupted)
            }
        }
    } else {
        ev.error || ev.hangup
    };

    if done {
        job.stdin_fd = None; // OwnedFd drop closes the pipe -> EOF for the child
        let _ = wait.remove(ev.fd);
        stdin_index.remove(&ev.fd);
    }
}

/// Per-tick CGI bookkeeping, run once per epoll_wait pass alongside the
/// client timeout sweep: kill jobs past their deadline, attempt a
/// non-blocking reap for anything not yet reaped, and finalize (deliver a
/// response, tear down fds) anything that's both reaped and either finished
/// normally (stdout EOF) or been killed.
fn reap_cgi(
    wait: &WaitSet,
    jobs: &mut HashMap<RawFd, CgiJob>,
    stdin_index: &mut HashMap<RawFd, RawFd>,
    peers: &mut HashMap<RawFd, Peer>,
    now: Instant,
) {
    let mut finished: Vec<RawFd> = Vec::new();

    for (job_id, job) in jobs.iter_mut() {
        if !job.abandoned && now >= job.deadline {
            eprintln!("localhost: cgi pid {} timed out, killing", job.pid);
            job.abandoned = true;
        }

        if job.abandoned && !job.kill_sent {
            unsafe {
                libc::kill(job.pid, libc::SIGKILL);
            }
            job.kill_sent = true;
        }

        if !job.reaped {
            let mut status: c_int = 0;
            let r = unsafe { libc::waitpid(job.pid, &mut status, libc::WNOHANG) };
            if r == job.pid {
                job.reaped = true;
            } else if r < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::ECHILD) {
                    job.reaped = true;
                }
            }
        }

        if job.reaped && (job.kill_sent || job.stdout_eof) {
            finished.push(*job_id);
        }
    }

    for job_id in finished {
        let Some(job) = jobs.remove(&job_id) else {
            continue;
        };
        if let Some(stdin) = &job.stdin_fd {
            let fd = stdin.as_raw_fd();
            let _ = wait.remove(fd);
            stdin_index.remove(&fd);
        }
        let _ = wait.remove(job_id); // job_id is stdout_fd's raw value

        let resp = if job.kill_sent {
            match dispatch::select_site(&job.sites, job.listen, job.host.as_deref()) {
                Some(site) => content::site_error(site, Status::GATEWAY_TIMEOUT),
                None => Outbound::error(Status::GATEWAY_TIMEOUT),
            }
        } else {
            content::finish_cgi(&job.out_buf, job.head_only)
        };

        if let Some(peer) = peers.get_mut(&job.owner) {
            peer.finish_cgi(job.cookie.as_deref(), resp);
            if let Err(e) = wait.modify(job.owner, Interest::write_only(), job.owner as u64) {
                eprintln!(
                    "localhost: epoll_ctl MOD peer {} after cgi: {e}",
                    job.owner
                );
            }
        }
        // else: owner peer already gone (client disconnected mid-CGI) —
        // nothing left to deliver to.
    }
}
