//! HTTP/1.1 message decode/encode (no routing yet).

mod codes;
mod decode;
mod encode;

pub use codes::Status;
#[allow(unused_imports)]
pub use codes::reason_phrase;
pub use decode::{try_parse, DecodeError};
#[allow(unused_imports)]
pub use decode::Inbound;
pub use encode::Outbound;
