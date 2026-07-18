//! Document-root file serving, MIME types, and custom error pages.

mod mime;
mod map;
mod serve;
mod errpage;

pub use errpage::site_error;
pub use serve::serve_get;
