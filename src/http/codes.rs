#![allow(dead_code)]
//! Status codes and stock error HTML.

pub struct Status;

impl Status {
    pub const OK: u16 = 200;
    pub const CREATED: u16 = 201;
    pub const NO_CONTENT: u16 = 204;
    pub const BAD_REQUEST: u16 = 400;
    pub const FORBIDDEN: u16 = 403;
    pub const NOT_FOUND: u16 = 404;
    pub const METHOD_NOT_ALLOWED: u16 = 405;
    pub const PAYLOAD_TOO_LARGE: u16 = 413;
    pub const INTERNAL: u16 = 500;
}

pub fn reason_phrase(code: u16) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        _ => "Error",
    }
}

pub fn default_error_html(code: u16) -> String {
    let reason = reason_phrase(code);
    format!(
        "<!DOCTYPE html>\n\
         <html><head><title>{code} {reason}</title></head>\n\
         <body><h1>{code} {reason}</h1>\n\
         <p>localhost</p></body></html>\n"
    )
}
