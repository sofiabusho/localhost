//! TCP listen sockets: bind, listen, non-blocking accept.

mod sock;

pub use sock::{Listener, open_listeners};

