//! Thin epoll wait-set around libc (single interest set for the process).

mod waitset;

#[allow(unused_imports)]
pub use waitset::{Interest, Ready, WaitSet};

