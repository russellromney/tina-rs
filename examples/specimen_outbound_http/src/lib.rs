//! Tina-vs-Tokio: outbound HTTP. Each side hosts a counter
//! HTTP server and drives it via its own outbound HTTP client.
//!
//! - Tokio: `axum` server + `reqwest` client.
//! - Tina: `tina_http::HttpListener` server + `tina_http::HttpClient`,
//!   bridged to the host thread via a tiny `Driver` isolate that
//!   forwards each call's outcome through an `std::sync::mpsc`.
//!
//! Read [`tokio_impl`] and [`tina_impl`] top-to-bottom; the README
//! compares feel.

pub mod tina_impl;
pub mod tokio_impl;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub successful_get: u32,
    pub successful_post: u32,
    pub final_counter_value: u32,
    pub got_404_for_missing: bool,
    pub exit_clean: bool,
}
