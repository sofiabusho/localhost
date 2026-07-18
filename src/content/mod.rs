//! Document-root file serving, uploads, deletes, CGI, and custom error pages.

mod mime;
mod map;
mod serve;
mod errpage;
mod upload;
mod remove;
mod cgi;

pub use cgi::{handle as handle_cgi, matches_route as cgi_matches};
pub use errpage::site_error;
pub use remove::handle_delete;
pub use serve::serve_get;
pub use upload::handle_post;
