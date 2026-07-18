//! Process-wide I/O hub: one `epoll_wait` drives listeners (and later peers).

use crate::ingress::Listener;
use crate::multiplex::{Interest, WaitSet};
use crate::settings::SiteBundle;
use libc::epoll_event;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::os::fd::{AsRawFd, RawFd};
use std::time::Duration;

/// Run until interrupted. Phase 2: accept then immediately close clients.
pub fn run(bundle: &SiteBundle) -> Result<(), String> {
    let mut addrs = Vec::new();
    for site in &bundle.sites {
        addrs.extend(site.binds.iter().copied());
    }

    let listeners = crate::ingress::open_listeners(&addrs)?;
    let wait = WaitSet::create().map_err(|e| format!("epoll_create1 failed: {e}"))?;

    let mut by_fd: HashMap<RawFd, Listener> = HashMap::new();
    for lst in listeners {
        let fd = lst.as_raw_fd();
        wait.add(fd, Interest::read_only(), fd as u64)
            .map_err(|e| format!("epoll_ctl ADD listener {fd}: {e}"))?;
        eprintln!("localhost: listening on {}", lst.addr());
        by_fd.insert(fd, lst);
    }

    let mut events = vec![unsafe { std::mem::zeroed::<epoll_event>() }; 64];
    eprintln!(
        "localhost: hub running ({} listener(s)); Ctrl-C to stop",
        by_fd.len()
    );

    loop {
        // Sole blocking wait-point for all sockets in this process.
        let ready = wait
            .wait(&mut events, Duration::from_millis(500).as_millis() as i32)
            .map_err(|e| format!("epoll_wait failed: {e}"))?;

        for ev in ready {
            if let Some(listener) = by_fd.get(&ev.fd) {
                if ev.readable {
                    accept_drain(listener);
                }
            }
            // Phase 3+: peer fds handled here with one read/write attempt each.
        }
    }
}

fn accept_drain(listener: &Listener) {
    loop {
        match listener.accept_nonblocking() {
            Ok((client, peer)) => {
                // Phase 2 proof: accept works; drop immediately (close).
                eprintln!("localhost: accepted {peer} (fd {}) — closed (phase 2)", client.as_raw_fd());
                drop(client);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("localhost: accept error on {}: {e}", listener.addr());
                break;
            }
        }
    }
}

