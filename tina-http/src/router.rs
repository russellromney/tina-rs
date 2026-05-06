//! Tiny routing helper for HTTP services.
//!
//! Maps `(Method, path)` to a stateless `fn(&HttpRequest) -> HttpResponse`
//! handler. Used inside a service's `handle`:
//!
//! ```rust,ignore
//! use tina_http::{Router, HttpRequest, HttpResponse};
//! use http::{Method, StatusCode};
//!
//! fn get_counter(_: &HttpRequest) -> HttpResponse {
//!     HttpResponse::with_status(StatusCode::OK)
//! }
//!
//! let router = Router::new().route(Method::GET, "/counter", get_counter);
//! let response = router.dispatch(&request);
//! ```
//!
//! Lookup is O(N) linear scan; first match wins. Path comparison is
//! exact — `/hi/` does not match `/hi`. `HEAD` is not implicitly
//! routed to `GET`. A miss on path or method both produce the
//! fallback (default `404`); first form does not distinguish 404 from
//! 405.
//!
//! Stateless. No middleware. No path params. For stateful routes the
//! user writes `match (request.method, request.path.as_str())` in
//! their service handler — same shape, slightly more code per arm.
//! For routes that forward to another isolate (e.g., upstream HTTP),
//! see the connection-isolate service-shape note in the phase plan.

use http::Method;

use crate::types::{HttpRequest, HttpResponse};

/// Stateless route handler.
pub type RouteHandler = fn(&HttpRequest) -> HttpResponse;

/// Method + path + handler triple.
#[derive(Clone)]
struct Route {
    method: Method,
    path: String,
    handler: RouteHandler,
}

/// Linear-scan router. First match wins. 404 fallback baked in.
#[derive(Clone)]
pub struct Router {
    routes: Vec<Route>,
    not_found: RouteHandler,
}

fn default_not_found(_: &HttpRequest) -> HttpResponse {
    HttpResponse::not_found()
}

impl Router {
    /// Builds an empty router with the default 404 fallback.
    pub fn new() -> Self {
        Self {
            routes: Vec::new(),
            not_found: default_not_found,
        }
    }

    /// Adds a route. Routes are matched in insertion order; the first
    /// `(method, path)` match wins.
    pub fn route(mut self, method: Method, path: impl Into<String>, handler: RouteHandler) -> Self {
        self.routes.push(Route {
            method,
            path: path.into(),
            handler,
        });
        self
    }

    /// Replaces the 404 fallback.
    pub fn fallback(mut self, handler: RouteHandler) -> Self {
        self.not_found = handler;
        self
    }

    /// Looks up the request. Returns the matched handler's response, or
    /// the fallback's response on miss.
    pub fn dispatch(&self, request: &HttpRequest) -> HttpResponse {
        for route in &self.routes {
            if request.method == route.method && request.path == route.path {
                return (route.handler)(request);
            }
        }
        (self.not_found)(request)
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::StatusCode;

    fn say_hi(_: &HttpRequest) -> HttpResponse {
        HttpResponse::text("hi")
    }

    fn say_bye(_: &HttpRequest) -> HttpResponse {
        HttpResponse::text("bye")
    }

    #[test]
    fn matches_method_and_path() {
        let router =
            Router::new()
                .route(Method::GET, "/hi", say_hi)
                .route(Method::POST, "/bye", say_bye);

        let req = HttpRequest::get("/hi").build();
        let resp = router.dispatch(&req);
        assert_eq!(resp.status, StatusCode::OK);
        assert_eq!(resp.body, b"hi");

        let req = HttpRequest::post("/bye").build();
        let resp = router.dispatch(&req);
        assert_eq!(resp.body, b"bye");
    }

    #[test]
    fn unknown_path_falls_through_to_404() {
        let router = Router::new().route(Method::GET, "/hi", say_hi);
        let req = HttpRequest::get("/missing").build();
        let resp = router.dispatch(&req);
        assert_eq!(resp.status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn method_mismatch_falls_through_to_404() {
        let router = Router::new().route(Method::GET, "/hi", say_hi);
        let req = HttpRequest::post("/hi").build();
        let resp = router.dispatch(&req);
        assert_eq!(resp.status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn first_match_wins() {
        fn first(_: &HttpRequest) -> HttpResponse {
            HttpResponse::text("first")
        }
        fn second(_: &HttpRequest) -> HttpResponse {
            HttpResponse::text("second")
        }
        let router = Router::new()
            .route(Method::GET, "/x", first)
            .route(Method::GET, "/x", second);
        let req = HttpRequest::get("/x").build();
        assert_eq!(router.dispatch(&req).body, b"first");
    }

    #[test]
    fn custom_fallback_overrides_default_404() {
        fn teapot(_: &HttpRequest) -> HttpResponse {
            HttpResponse::with_status(StatusCode::IM_A_TEAPOT)
        }
        let router = Router::new().fallback(teapot);
        let req = HttpRequest::get("/anything").build();
        let resp = router.dispatch(&req);
        assert_eq!(resp.status, StatusCode::IM_A_TEAPOT);
    }
}
