//! Accepted client sockets and their I/O state machine.

mod link;

pub use link::{CgiHandoff, Peer, PeerAction, PeerOutcome, Timing};
