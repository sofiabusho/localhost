//! Accepted client sockets and their I/O state machine.

mod link;

pub use link::{Peer, PeerAction, PeerOutcome, Timing};
