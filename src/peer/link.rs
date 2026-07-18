#![allow(dead_code)] // listen_addr used from Phase 5
//! Per-client buffers, phases, and timeout bookkeeping.

use std::io::{self, ErrorKind};
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

/// Soft limits for slow / idle clients (Phase 3).
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// Max time from accept until a full request head is seen.
    pub request: Duration,
    /// Max time between successful I/O activity.
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
    /// Gathering bytes until headers terminator (stub until Phase 4).
    Recv,
    /// Flushing a prepared response.
    Send,
}

/// What the hub should do to epoll interest after a step.
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

const MAX_IN: usize = 64 * 1024;
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
}

impl Peer {
    pub fn new(fd: OwnedFd, peer_addr: SocketAddr, listen_addr: SocketAddr, timing: Timing) -> Self {
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
            Ok(0) => {
                // Peer closed before a complete request.
                return PeerOutcome::Drop;
            }
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
            // Oversized head — close for now; Phase 4 maps this to 413.
            return PeerOutcome::Drop;
        }
        self.inbuf.extend_from_slice(&tmp[..n]);

        if headers_complete(&self.inbuf) {
            self.prepare_stub_response();
            self.phase = Phase::Send;
            PeerOutcome::Ok(PeerAction::WantSend)
        } else {
            PeerOutcome::Ok(PeerAction::KeepRecv)
        }
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
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                PeerOutcome::Ok(PeerAction::WantSend)
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {
                PeerOutcome::Ok(PeerAction::WantSend)
            }
            Err(_) => PeerOutcome::Drop,
        }
    }

    fn prepare_stub_response(&mut self) {
        // Phase 3 debug response — replaced by real HTTP builder in Phase 4.
        let body = b"localhost phase3: request head received\n";
        let mut msg = Vec::new();
        msg.extend_from_slice(b"HTTP/1.1 200 OK\r\n");
        msg.extend_from_slice(b"Content-Type: text/plain; charset=utf-8\r\n");
        msg.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
        msg.extend_from_slice(b"Connection: close\r\n");
        msg.extend_from_slice(b"\r\n");
        msg.extend_from_slice(body);
        self.outbuf = msg;
        self.out_off = 0;
    }
}

fn headers_complete(buf: &[u8]) -> bool {
    buf.windows(4).any(|w| w == b"\r\n\r\n")
}

fn recv_once(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    let n = unsafe {
        libc::recv(
            fd,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            0,
        )
    };
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

#[cfg(test)]
mod tests {
    use super::headers_complete;

    #[test]
    fn detects_header_terminator() {
        assert!(!headers_complete(b"GET / HTTP/1.1\r\nHost: x\r\n"));
        assert!(headers_complete(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"));
    }
}
