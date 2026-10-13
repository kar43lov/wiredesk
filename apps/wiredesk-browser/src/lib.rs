#[cfg(any(target_os = "macos", test))]
mod http;
#[cfg(target_os = "macos")]
mod service;
#[cfg(target_os = "macos")]
pub use service::Service;
#[cfg(target_os = "macos")]
pub mod desktop_client;
#[cfg(any(target_os = "macos", test))]
pub mod web_forwarder;

pub const VIEWER_URL: &str = "http://wiredesk.localhost/";
pub const VIEWER_URL_WITH_PORT: &str = "http://wiredesk.localhost:19223/";
pub const CDP_URL: &str = "http://127.0.0.1:19222";
