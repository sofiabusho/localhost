//! Low-level non-blocking TCP listeners.

use std::io::{self, Error};
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// A listening TCP socket registered later with the wait-set.
pub struct Listener {
    fd: OwnedFd,
    addr: SocketAddr,
}

impl Listener {
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let family = match addr {
            SocketAddr::V4(_) => libc::AF_INET,
            SocketAddr::V6(_) => libc::AF_INET6,
        };
        let fd = unsafe { libc::socket(family, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(Error::last_os_error());
        }
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };

        set_reuseaddr(owned.as_raw_fd())?;
        set_nonblocking(owned.as_raw_fd())?;

        match addr {
            SocketAddr::V4(v4) => {
                let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
                sa.sin_family = libc::AF_INET as libc::sa_family_t;
                sa.sin_port = u16::to_be(v4.port());
                sa.sin_addr = libc::in_addr {
                    s_addr: u32::from(*v4.ip()).to_be(),
                };
                let rc = unsafe {
                    libc::bind(
                        owned.as_raw_fd(),
                        &sa as *const _ as *const libc::sockaddr,
                        std::mem::size_of_val(&sa) as libc::socklen_t,
                    )
                };
                if rc < 0 {
                    return Err(Error::last_os_error());
                }
            }
            SocketAddr::V6(v6) => {
                let mut sa: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
                sa.sin6_family = libc::AF_INET6 as libc::sa_family_t;
                sa.sin6_port = u16::to_be(v6.port());
                sa.sin6_addr.s6_addr = v6.ip().octets();
                let rc = unsafe {
                    libc::bind(
                        owned.as_raw_fd(),
                        &sa as *const _ as *const libc::sockaddr,
                        std::mem::size_of_val(&sa) as libc::socklen_t,
                    )
                };
                if rc < 0 {
                    return Err(Error::last_os_error());
                }
            }
        }

        let rc = unsafe { libc::listen(owned.as_raw_fd(), 128) };
        if rc < 0 {
            return Err(Error::last_os_error());
        }

        Ok(Self { fd: owned, addr })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Accept one connection. Returns `WouldBlock` when the queue is empty.
    pub fn accept_nonblocking(&self) -> io::Result<(OwnedFd, SocketAddr)> {
        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of_val(&storage) as libc::socklen_t;
        let client = unsafe {
            libc::accept4(
                self.fd.as_raw_fd(),
                &mut storage as *mut _ as *mut libc::sockaddr,
                &mut len,
                libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            )
        };
        if client < 0 {
            return Err(Error::last_os_error());
        }
        let peer = sockaddr_to_addr(&storage, len)?;
        let owned = unsafe { OwnedFd::from_raw_fd(client) };
        Ok((owned, peer))
    }
}

/// Open every unique bind address. Failures on individual binds are collected;
/// at least one success is required.
pub fn open_listeners(addrs: &[SocketAddr]) -> Result<Vec<Listener>, String> {
    let mut unique = Vec::new();
    for a in addrs {
        if !unique.contains(a) {
            unique.push(*a);
        }
    }
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for addr in unique {
        match Listener::bind(addr) {
            Ok(l) => out.push(l),
            Err(e) => errors.push(format!("{addr}: {e}")),
        }
    }
    if out.is_empty() {
        return Err(format!(
            "failed to bind any listen address: {}",
            errors.join("; ")
        ));
    }
    for e in errors {
        eprintln!("localhost: warning: listen skipped: {e}");
    }
    Ok(out)
}

fn set_reuseaddr(fd: RawFd) -> io::Result<()> {
    let on: libc::c_int = 1;
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            &on as *const _ as *const libc::c_void,
            std::mem::size_of_val(&on) as libc::socklen_t,
        )
    };
    if rc < 0 {
        Err(Error::last_os_error())
    } else {
        Ok(())
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(Error::last_os_error());
    }
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if rc < 0 {
        Err(Error::last_os_error())
    } else {
        Ok(())
    }
}

fn sockaddr_to_addr(
    storage: &libc::sockaddr_storage,
    _len: libc::socklen_t,
) -> io::Result<SocketAddr> {
    match storage.ss_family as i32 {
        libc::AF_INET => {
            let sin = unsafe { &*(storage as *const _ as *const libc::sockaddr_in) };
            let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            let port = u16::from_be(sin.sin_port);
            Ok(SocketAddr::from((ip, port)))
        }
        libc::AF_INET6 => {
            let sin6 = unsafe { &*(storage as *const _ as *const libc::sockaddr_in6) };
            let ip = std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr);
            let port = u16::from_be(sin6.sin6_port);
            Ok(SocketAddr::from((ip, port)))
        }
        other => Err(Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown address family {other}"),
        )),
    }
}
