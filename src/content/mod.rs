//! Document-root file serving, uploads, deletes, and custom error pages.

mod mime;
mod map;
mod serve;
mod errpage;
mod upload;
mod remove;

pub use errpage::site_error;
pub use remove::handle_delete;
pub use serve::serve_get;
pub use upload::handle_post;
