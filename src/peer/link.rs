#![allow(dead_code)]
//! Per-client buffers, phases, and timeout bookkeeping.

use crate::dispatch;
use crate::http::{try_parse, DecodeError, Outbound, Status};
use crate::settings::SiteBundle;
use std::io::{self, ErrorKind};
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Soft limits for slow / idle clients.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub request: Duration,
    pub idle: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            request: Duration::from_secs(30),
            idle: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Recv,
    Send,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerAction {
    KeepRecv,
    WantSend,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerOutcome {
    Ok(PeerAction),
    Drop,
}

const MAX_IN: usize = 1024 * 1024;
const READ_CHUNK: usize = 8 * 1024;
const WRITE_CHUNK: usize = 8 * 1024;

pub struct Peer {
    fd: OwnedFd,
    pub peer_addr: SocketAddr,
    pub listen_addr: SocketAddr,
    phase: Phase,
    inbuf: Vec<u8>,
    outbuf: Vec<u8>,
    out_off: usize,
    born: Instant,
    last_io: Instant,
    timing: Timing,
    max_body: u64,
    sites: Arc<SiteBundle>,
}

impl Peer {
    pub fn new(
        fd: OwnedFd,
        peer_addr: SocketAddr,
        listen_addr: SocketAddr,
        timing: Timing,
        max_body: u64,
        sites: Arc<SiteBundle>,
    ) -> Self {
        let now = Instant::now();
        Self {
            fd,
            peer_addr,
            listen_addr,
            phase: Phase::Recv,
            inbuf: Vec::with_capacity(1024),
            outbuf: Vec::new(),
            out_off: 0,
            born: now,
            last_io: now,
            timing,
            max_body,
            sites,
        }
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub fn timed_out(&self, now: Instant) -> bool {
        if now.duration_since(self.last_io) > self.timing.idle {
            return true;
        }
        if self.phase == Phase::Recv && now.duration_since(self.born) > self.timing.request {
            return true;
        }
        false
    }

    /// At most one `recv` syscall. Call only when epoll reported readable.
    pub fn on_readable(&mut self) -> PeerOutcome {
        if self.phase != Phase::Recv {
            return PeerOutcome::Ok(PeerAction::WantSend);
        }

        let mut tmp = [0u8; READ_CHUNK];
        let n = match recv_once(self.fd.as_raw_fd(), &mut tmp) {
            Ok(0) => return PeerOutcome::Drop,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                return PeerOutcome::Ok(PeerAction::KeepRecv);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {
                return PeerOutcome::Ok(PeerAction::KeepRecv);
            }
            Err(_) => return PeerOutcome::Drop,
        };

        self.last_io = Instant::now();
        if self.inbuf.len() + n > MAX_IN {
            self.reply(Outbound::error(Status::PAYLOAD_TOO_LARGE));
            return PeerOutcome::Ok(PeerAction::WantSend);
        }
        self.inbuf.extend_from_slice(&tmp[..n]);
        self.try_finish_request()
    }

    fn try_finish_request(&mut self) -> PeerOutcome {
        match try_parse(&self.inbuf, self.max_body) {
            Err(DecodeError::Incomplete) => PeerOutcome::Ok(PeerAction::KeepRecv),
            Err(DecodeError::BadRequest(_)) => {
                self.reply(Outbound::error(Status::BAD_REQUEST));
                PeerOutcome::Ok(PeerAction::WantSend)
            }
            Err(DecodeError::PayloadTooLarge) => {
                self.reply(Outbound::error(Status::PAYLOAD_TOO_LARGE));
                PeerOutcome::Ok(PeerAction::WantSend)
            }
            Ok((msg, _consumed)) => {
                let resp = dispatch::answer(self.listen_addr, &msg, &self.sites);
                self.reply(resp);
                PeerOutcome::Ok(PeerAction::WantSend)
            }
        }
    }

    fn reply(&mut self, resp: Outbound) {
        self.outbuf = resp.to_bytes();
        self.out_off = 0;
        self.phase = Phase::Send;
        self.inbuf.clear();
    }

    /// At most one `send` syscall. Call only when epoll reported writable.
    pub fn on_writable(&mut self) -> PeerOutcome {
        if self.phase != Phase::Send {
            return PeerOutcome::Ok(PeerAction::KeepRecv);
        }

        let remaining = &self.outbuf[self.out_off..];
        if remaining.is_empty() {
            return PeerOutcome::Ok(PeerAction::Close);
        }

        let want = remaining.len().min(WRITE_CHUNK);
        match send_once(self.fd.as_raw_fd(), &remaining[..want]) {
            Ok(0) => PeerOutcome::Drop,
            Ok(n) => {
                self.last_io = Instant::now();
                self.out_off += n;
                if self.out_off >= self.outbuf.len() {
                    PeerOutcome::Ok(PeerAction::Close)
                } else {
                    PeerOutcome::Ok(PeerAction::WantSend)
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => PeerOutcome::Ok(PeerAction::WantSend),
            Err(e) if e.kind() == ErrorKind::Interrupted => PeerOutcome::Ok(PeerAction::WantSend),
            Err(_) => PeerOutcome::Drop,
        }
    }
}

fn recv_once(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    let n = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

fn send_once(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    let n = unsafe {
        libc::send(
            fd,
            buf.as_ptr() as *const libc::c_void,
            buf.len(),
            libc::MSG_NOSIGNAL,
        )
    };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}
