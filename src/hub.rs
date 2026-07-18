//! Process-wide I/O hub: one `epoll_wait` drives listeners and peers.

use crate::ingress::Listener;
use crate::multiplex::{Interest, WaitSet};
use crate::peer::{Peer, PeerAction, PeerOutcome, Timing};
use crate::settings::SiteBundle;
use libc::epoll_event;
use std::collections::HashMap;
use std::io::ErrorKind;
use std::os::fd::{AsRawFd, RawFd};
use std::time::Instant;

const WAIT_SLICE_MS: i32 = 250;

/// Run until interrupted.
pub fn run(bundle: &SiteBundle) -> Result<(), String> {
    let mut addrs = Vec::new();
    for site in &bundle.sites {
        addrs.extend(site.binds.iter().copied());
    }

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
    let mut events = vec![unsafe { std::mem::zeroed::<epoll_event>() }; 64];
    eprintln!(
        "localhost: hub running ({} listener(s)); Ctrl-C to stop",
        listeners_by_fd.len()
    );

    loop {
        // Sole blocking wait-point for all sockets in this process.
        let ready = wait
            .wait(&mut events, WAIT_SLICE_MS)
            .map_err(|e| format!("epoll_wait failed: {e}"))?;

        let now = Instant::now();

        for ev in ready {
            if listeners_by_fd.contains_key(&ev.fd) {
                if ev.readable {
                    // Re-borrow separately to avoid holding listeners across peer mut.
                    accept_drain(&wait, &listeners_by_fd, &mut peers, ev.fd, timing);
                }
                continue;
            }

            let Some(peer) = peers.get_mut(&ev.fd) else {
                continue;
            };

            if ev.error || (ev.hangup && !ev.readable && !ev.writable) {
                drop_peer(&wait, &mut peers, ev.fd);
                continue;
            }

            // At most one read attempt and one write attempt per wake for this fd.
            let mut action = PeerAction::KeepRecv;
            let mut dead = false;

            if ev.readable {
                match peer.on_readable() {
                    PeerOutcome::Drop => dead = true,
                    PeerOutcome::Ok(a) => action = a,
                }
            }

            if !dead && ev.writable {
                match peer.on_writable() {
                    PeerOutcome::Drop => dead = true,
                    PeerOutcome::Ok(a) => action = a,
                }
            }

            if dead || action == PeerAction::Close {
                drop_peer(&wait, &mut peers, ev.fd);
                continue;
            }

            let interest = match action {
                PeerAction::KeepRecv => Interest::read_only(),
                PeerAction::WantSend => Interest::write_only(),
                PeerAction::Close => unreachable!(),
            };
            if let Err(e) = wait.modify(ev.fd, interest, ev.fd as u64) {
                eprintln!("localhost: epoll_ctl MOD peer {}: {e}", ev.fd);
                drop_peer(&wait, &mut peers, ev.fd);
            }
        }

        reap_timeouts(&wait, &mut peers, now);
    }
}

fn accept_drain(
    wait: &WaitSet,
    listeners: &HashMap<RawFd, Listener>,
    peers: &mut HashMap<RawFd, Peer>,
    listen_fd: RawFd,
    timing: Timing,
) {
    let Some(listener) = listeners.get(&listen_fd) else {
        return;
    };
    let listen_addr = listener.addr();

    loop {
        match listener.accept_nonblocking() {
            Ok((client, peer_addr)) => {
                let fd = client.as_raw_fd();
                if let Err(e) = wait.add(fd, Interest::read_only(), fd as u64) {
                    eprintln!("localhost: epoll_ctl ADD peer {fd}: {e}");
                    drop(client);
                    continue;
                }
                let peer = Peer::new(client, peer_addr, listen_addr, timing);
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

fn drop_peer(wait: &WaitSet, peers: &mut HashMap<RawFd, Peer>, fd: RawFd) {
    if peers.remove(&fd).is_some() {
        let _ = wait.remove(fd);
        // OwnedFd drop closes the socket.
    }
}

fn reap_timeouts(wait: &WaitSet, peers: &mut HashMap<RawFd, Peer>, now: Instant) {
    let expired: Vec<RawFd> = peers
        .iter()
        .filter_map(|(fd, p)| if p.timed_out(now) { Some(*fd) } else { None })
        .collect();
    for fd in expired {
        eprintln!("localhost: peer fd {fd} timed out");
        drop_peer(wait, peers, fd);
    }
}
