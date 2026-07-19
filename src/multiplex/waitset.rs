#![allow(dead_code)] // used by later phases
//! epoll_create1 / epoll_ctl / epoll_wait wrapper.

use libc::{
    self, c_int, epoll_event, EPOLLERR, EPOLLHUP, EPOLLIN, EPOLLOUT, EPOLL_CTL_ADD,
    EPOLL_CTL_DEL, EPOLL_CTL_MOD,
};
use std::io::{self, Error};
use std::os::fd::{AsRawFd, RawFd};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interest {
    pub read: bool,
    pub write: bool,
}

impl Interest {
    /// Neither readable nor writable — still yields EPOLLERR/EPOLLHUP (see
    /// `as_events`). Not currently used for CGI-wait peers: plain
    /// EPOLLERR/EPOLLHUP alone does not fire on an ordinary peer close()
    /// (confirmed empirically — only EPOLLIN via a recv() returning 0, or
    /// EPOLLRDHUP, catches that), so a peer waiting on a CGI job stays
    /// registered `read_only()` instead so it can still notice an abort.
    /// Kept as a documented, correct primitive for callers that genuinely
    /// only care about error/hangup conditions.
    pub fn none() -> Self {
        Self {
            read: false,
            write: false,
        }
    }

    pub fn read_only() -> Self {
        Self {
            read: true,
            write: false,
        }
    }

    pub fn write_only() -> Self {
        Self {
            read: false,
            write: true,
        }
    }

    pub fn read_write() -> Self {
        Self {
            read: true,
            write: true,
        }
    }

    fn as_events(self) -> u32 {
        let mut e = 0u32;
        if self.read {
            e |= EPOLLIN as u32;
        }
        if self.write {
            e |= EPOLLOUT as u32;
        }
        // Level-triggered: stays ready until the condition clears, so the hub
        // can honor "one read/write attempt per wake" without missing bytes.
        e | (EPOLLERR as u32) | (EPOLLHUP as u32)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Ready {
    pub fd: RawFd,
    pub readable: bool,
    pub writable: bool,
    pub hangup: bool,
    pub error: bool,
}

/// One epoll instance owned by the event hub.
pub struct WaitSet {
    epfd: RawFd,
}

impl WaitSet {
    pub fn create() -> io::Result<Self> {
        let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epfd < 0 {
            return Err(Error::last_os_error());
        }
        Ok(Self { epfd })
    }

    pub fn add(&self, fd: RawFd, interest: Interest, token: u64) -> io::Result<()> {
        self.ctl(EPOLL_CTL_ADD, fd, interest, token)
    }

    pub fn modify(&self, fd: RawFd, interest: Interest, token: u64) -> io::Result<()> {
        self.ctl(EPOLL_CTL_MOD, fd, interest, token)
    }

    pub fn remove(&self, fd: RawFd) -> io::Result<()> {
        let rc = unsafe { libc::epoll_ctl(self.epfd, EPOLL_CTL_DEL, fd, std::ptr::null_mut()) };
        if rc < 0 {
            return Err(Error::last_os_error());
        }
        Ok(())
    }

    fn ctl(&self, op: c_int, fd: RawFd, interest: Interest, token: u64) -> io::Result<()> {
        let mut ev = epoll_event {
            events: interest.as_events(),
            u64: token,
        };
        let rc = unsafe { libc::epoll_ctl(self.epfd, op, fd, &mut ev) };
        if rc < 0 {
            return Err(Error::last_os_error());
        }
        Ok(())
    }

    /// Block until readiness, or until `timeout_ms` elapses (`-1` = forever).
    pub fn wait(&self, buf: &mut [epoll_event], timeout_ms: i32) -> io::Result<Vec<Ready>> {
        if buf.is_empty() {
            return Ok(Vec::new());
        }
        let n = unsafe {
            libc::epoll_wait(
                self.epfd,
                buf.as_mut_ptr(),
                buf.len() as c_int,
                timeout_ms,
            )
        };
        if n < 0 {
            let err = Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                return Ok(Vec::new());
            }
            return Err(err);
        }
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n as usize {
            let ev = buf[i];
            let events = ev.events;
            out.push(Ready {
                fd: ev.u64 as RawFd,
                readable: events & EPOLLIN as u32 != 0,
                writable: events & EPOLLOUT as u32 != 0,
                hangup: events & EPOLLHUP as u32 != 0,
                error: events & EPOLLERR as u32 != 0,
            });
        }
        Ok(out)
    }
}

impl Drop for WaitSet {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.epfd);
        }
    }
}

impl AsRawFd for WaitSet {
    fn as_raw_fd(&self) -> RawFd {
        self.epfd
    }
}
