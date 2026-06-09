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
///
/// Convenience constructors `get`/`post`/`put`/`delete`/`patch`
/// exist alongside the generic `route(method, path, handler)`. With
/// [`Self::method_not_allowed`] the router distinguishes 404 (path
/// unknown) from 405 (method mismatch on a known path).
#[derive(Clone)]
pub struct Router {
    routes: Vec<Route>,
    not_found: RouteHandler,
    distinguish_method: bool,
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
            distinguish_method: false,
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

    /// Sugar for `route(Method::GET, path, handler)`.
    pub fn get(self, path: impl Into<String>, handler: RouteHandler) -> Self {
        self.route(Method::GET, path, handler)
    }

    /// Sugar for `route(Method::POST, path, handler)`.
    pub fn post(self, path: impl Into<String>, handler: RouteHandler) -> Self {
        self.route(Method::POST, path, handler)
    }

    /// Sugar for `route(Method::PUT, path, handler)`.
    pub fn put(self, path: impl Into<String>, handler: RouteHandler) -> Self {
        self.route(Method::PUT, path, handler)
    }

    /// Sugar for `route(Method::DELETE, path, handler)`.
    pub fn delete(self, path: impl Into<String>, handler: RouteHandler) -> Self {
        self.route(Method::DELETE, path, handler)
    }

    /// Sugar for `route(Method::PATCH, path, handler)`.
    pub fn patch(self, path: impl Into<String>, handler: RouteHandler) -> Self {
        self.route(Method::PATCH, path, handler)
    }

    /// Replaces the default 404 fallback.
    ///
    /// Use [`Self::method_not_allowed`] if you want path-known but
    /// method-mismatched requests to surface as 405 instead of being
    /// folded into the 404 fallback.
    pub fn fallback(mut self, handler: RouteHandler) -> Self {
        self.not_found = handler;
        self
    }

    /// Switches the router to *path-aware* miss semantics: path
    /// known but method not allowed → `405 Method Not Allowed`;
    /// path not known → `404` (or the [`fallback`](Self::fallback)
    /// handler).
    pub fn method_not_allowed(mut self) -> Self {
        self.distinguish_method = true;
        self
    }

    /// Looks up the request. Returns the matched handler's response,
    /// or the fallback's response on miss. With
    /// [`method_not_allowed`](Self::method_not_allowed) enabled,
    /// returns `405` for path-known method-mismatched requests.
    pub fn dispatch(&self, request: &HttpRequest) -> HttpResponse {
        let mut path_seen = false;
        for route in &self.routes {
            if request.path == route.path {
                if request.method == route.method {
                    return (route.handler)(request);
                }
                path_seen = true;
            }
        }
        if path_seen && self.distinguish_method {
            return HttpResponse::with_status(http::StatusCode::METHOD_NOT_ALLOWED);
        }
        (self.not_found)(request)
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

/// Stateful sibling of [`Router`]. Dispatches `(method, path)` to a
/// `fn(&mut S, &HttpRequest) -> HttpResponse` so an isolate's
/// `handle` can route between handlers that mutate isolate state.
///
/// ```ignore
/// fn get_counter(c: &mut Counter, _: &HttpRequest) -> HttpResponse {
///     HttpResponse::text(c.value.to_string())
/// }
/// fn post_counter(c: &mut Counter, _: &HttpRequest) -> HttpResponse {
///     c.value += 1;
///     HttpResponse::text(c.value.to_string())
/// }
///
/// let response = StatefulRouter::<Counter>::new()
///     .get("/counter", get_counter)
///     .post("/counter", post_counter)
///     .dispatch(self, &request);
/// ```
///
/// Cheap to construct per handler turn; routes are `fn` pointers so
/// the type stays `Copy`-ish and never owns state.
pub type StatefulHandler<S> = fn(&mut S, &HttpRequest) -> HttpResponse;

#[derive(Clone)]
struct StatefulRoute<S: 'static> {
    method: Method,
    path: String,
    handler: StatefulHandler<S>,
}

/// In-isolate router with `&mut S` access. See module docs.
#[derive(Clone)]
pub struct StatefulRouter<S: 'static> {
    routes: Vec<StatefulRoute<S>>,
    not_found: StatefulHandler<S>,
    distinguish_method: bool,
}

fn default_stateful_not_found<S>(_: &mut S, _: &HttpRequest) -> HttpResponse {
    HttpResponse::not_found()
}

impl<S: 'static> StatefulRouter<S> {
    /// Build an empty router with the default 404 fallback.
    pub fn new() -> Self {
        Self {
            routes: Vec::new(),
            not_found: default_stateful_not_found::<S>,
            distinguish_method: false,
        }
    }

    /// Adds a route by explicit method.
    pub fn route(
        mut self,
        method: Method,
        path: impl Into<String>,
        handler: StatefulHandler<S>,
    ) -> Self {
        self.routes.push(StatefulRoute {
            method,
            path: path.into(),
            handler,
        });
        self
    }

    /// Sugar for `route(Method::GET, ...)`.
    pub fn get(self, path: impl Into<String>, handler: StatefulHandler<S>) -> Self {
        self.route(Method::GET, path, handler)
    }

    /// Sugar for `route(Method::POST, ...)`.
    pub fn post(self, path: impl Into<String>, handler: StatefulHandler<S>) -> Self {
        self.route(Method::POST, path, handler)
    }

    /// Sugar for `route(Method::PUT, ...)`.
    pub fn put(self, path: impl Into<String>, handler: StatefulHandler<S>) -> Self {
        self.route(Method::PUT, path, handler)
    }

    /// Sugar for `route(Method::DELETE, ...)`.
    pub fn delete(self, path: impl Into<String>, handler: StatefulHandler<S>) -> Self {
        self.route(Method::DELETE, path, handler)
    }

    /// Sugar for `route(Method::PATCH, ...)`.
    pub fn patch(self, path: impl Into<String>, handler: StatefulHandler<S>) -> Self {
        self.route(Method::PATCH, path, handler)
    }

    /// Replace the default 404 fallback.
    pub fn fallback(mut self, handler: StatefulHandler<S>) -> Self {
        self.not_found = handler;
        self
    }

    /// Distinguish 405 from 404 (see [`Router::method_not_allowed`]).
    pub fn method_not_allowed(mut self) -> Self {
        self.distinguish_method = true;
        self
    }

    /// Dispatch the request through the registered handlers.
    pub fn dispatch(&self, state: &mut S, request: &HttpRequest) -> HttpResponse {
        let mut path_seen = false;
        for route in &self.routes {
            if request.path == route.path {
                if request.method == route.method {
                    return (route.handler)(state, request);
                }
                path_seen = true;
            }
        }
        if path_seen && self.distinguish_method {
            return HttpResponse::with_status(http::StatusCode::METHOD_NOT_ALLOWED);
        }
        (self.not_found)(state, request)
    }
}

impl<S: 'static> Default for StatefulRouter<S> {
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

    #[test]
    fn sugar_methods_match_route_method() {
        let router = Router::new()
            .get("/hi", say_hi)
            .post("/bye", say_bye)
            .put("/up", say_hi)
            .delete("/gone", say_bye)
            .patch("/fix", say_hi);
        for (method, path, body) in [
            (Method::GET, "/hi", "hi"),
            (Method::POST, "/bye", "bye"),
            (Method::PUT, "/up", "hi"),
            (Method::DELETE, "/gone", "bye"),
            (Method::PATCH, "/fix", "hi"),
        ] {
            let req = HttpRequest::builder(method.clone(), path).build();
            let resp = router.dispatch(&req);
            assert_eq!(resp.status, StatusCode::OK, "method {method:?} path {path}");
            assert_eq!(resp.body, body.as_bytes());
        }
    }

    #[test]
    fn method_not_allowed_distinguishes_405_from_404() {
        let router = Router::new().get("/hi", say_hi).method_not_allowed();

        let req = HttpRequest::post("/hi").build();
        let resp = router.dispatch(&req);
        assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED);

        let req = HttpRequest::get("/missing").build();
        let resp = router.dispatch(&req);
        assert_eq!(resp.status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn method_not_allowed_off_by_default_keeps_404() {
        let router = Router::new().get("/hi", say_hi);
        let req = HttpRequest::post("/hi").build();
        let resp = router.dispatch(&req);
        assert_eq!(resp.status, StatusCode::NOT_FOUND);
    }

    #[derive(Debug, Default)]
    struct CounterState {
        value: u32,
    }

    fn get_counter(state: &mut CounterState, _: &HttpRequest) -> HttpResponse {
        HttpResponse::text(state.value.to_string())
    }

    fn post_counter(state: &mut CounterState, _: &HttpRequest) -> HttpResponse {
        state.value += 1;
        HttpResponse::text(state.value.to_string())
    }

    #[test]
    fn stateful_router_dispatches_with_mut_state() {
        let router = StatefulRouter::<CounterState>::new()
            .get("/counter", get_counter)
            .post("/counter", post_counter);

        let mut state = CounterState::default();
        let resp = router.dispatch(&mut state, &HttpRequest::get("/counter").build());
        assert_eq!(resp.body, b"0");

        let resp = router.dispatch(&mut state, &HttpRequest::post("/counter").build());
        assert_eq!(resp.body, b"1");

        let resp = router.dispatch(&mut state, &HttpRequest::get("/counter").build());
        assert_eq!(resp.body, b"1");
    }

    #[test]
    fn stateful_router_404_and_405_match_router_semantics() {
        let mut state = CounterState::default();
        let router = StatefulRouter::<CounterState>::new()
            .get("/counter", get_counter)
            .method_not_allowed();

        let resp = router.dispatch(&mut state, &HttpRequest::post("/counter").build());
        assert_eq!(resp.status, StatusCode::METHOD_NOT_ALLOWED);

        let resp = router.dispatch(&mut state, &HttpRequest::get("/missing").build());
        assert_eq!(resp.status, StatusCode::NOT_FOUND);
    }
}
