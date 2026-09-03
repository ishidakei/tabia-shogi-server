//! The 500 a panicking handler answers with, and the record it leaves.
//!
//! Hyper does not turn a panicking service future into a status: the panic
//! unwinds the connection task, the socket closes with no byte on it, and a
//! reader sees a connection error rather than a response. The web half has no
//! supervisor of its own, so without this module a handler panic would go both
//! unanswered and unlogged.
//!
//! The catch is [`tower_http::catch_panic`], which is the whole reason
//! `tower-http` is a dependency. What it does not do is tell the panic handler
//! which request panicked: [`ResponseForPanic::response_for_panic`] is given the
//! panic payload and nothing else, and the handler the layer holds is one value
//! shared by every request. So the layer is built per request instead —
//! [`Catching`] reads the route and the method off the request on its way past,
//! hands them to a [`Panicked`] made for that request alone, and wraps the
//! route's service in `CatchPanicLayer::custom` with it.
//!
//! The route is the template, not the path — [`matched_route`], shared with the
//! render measurement so that a panic record and a measured distribution name a
//! route the same way.
//!
//! A panic payload is written by whatever code panicked and can quote a
//! condition or a value, so the response is the fixed [`BODY`] line under
//! `text/plain; charset=utf-8`, with no payload, no backtrace and no header
//! carrying either. The payload reaches the operator through the default panic
//! hook's line on stderr.
//!
//! [`matched_route`]: super::matched_route

use std::any::Any;
use std::mem;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::Route;
use tower::{Layer, Service};
use tower_http::catch_panic::{CatchPanic, CatchPanicLayer, ResponseForPanic};
use tracing::error;

/// What a reader is told, and the whole of it.
pub const BODY: &str = "internal server error";

/// The layer the router is wrapped in, outermost.
///
/// Outermost is load-bearing: a layer added later panicking is as much a closed
/// connection as a handler panicking, and only what sits outside it catches
/// that. `Router::layer` puts the last layer applied outermost, so this one is
/// applied last; see [`router`](super::routes::router).
#[derive(Clone, Copy, Debug)]
pub struct CatchPanics;

impl Layer<Route> for CatchPanics {
    type Service = Catching;

    fn layer(&self, inner: Route) -> Self::Service {
        Catching { inner }
    }
}

/// One route's service, with a per-request catch around it.
#[derive(Clone, Debug)]
pub struct Catching {
    inner: Route,
}

/// What a request actually goes through: tower-http's middleware, holding a
/// handler that knows this request's route.
///
/// Named so that the three associated types below can be projected out of it
/// rather than spelled: naming the body tower-http boxes into would mean
/// depending on `http-body-util` and `bytes` directly.
type Caught = CatchPanic<Route, Panicked>;

impl Service<Request> for Catching {
    type Response = <Caught as Service<Request>>::Response;
    type Error = <Caught as Service<Request>>::Error;
    type Future = <Caught as Service<Request>>::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Spelled out rather than `self.inner.poll_ready(cx)`, because `Route`
        // is a `Service` over every body type and this one is the request type
        // this service is implemented for.
        <Route as Service<Request>>::poll_ready(&mut self.inner, cx)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        let panicked = Panicked::of(&request);
        // The readied service is the one that must take the request, so it is
        // moved out and a fresh clone left in its place: a service that was not
        // always ready would be called unready by the other order.
        let spare = self.inner.clone();
        let ready = mem::replace(&mut self.inner, spare);

        CatchPanicLayer::custom(panicked).layer(ready).call(request)
    }
}

/// One request's answer to a panic: the route it was on, and the method.
///
/// `pub` because it is named by [`Catching`]'s associated types, which are the
/// tower-http middleware's own; nothing outside this module constructs one.
#[derive(Clone, Debug)]
pub struct Panicked {
    /// The method, which distinguishes two panics on one route taking more than
    /// one.
    method: Method,

    /// The matched route template, or the path for a request that matched none.
    route: String,
}

impl Panicked {
    /// Reads the request's identity, before the request is handed on.
    fn of(request: &Request) -> Self {
        Self {
            method: request.method().clone(),
            route: super::matched_route(request),
        }
    }
}

impl ResponseForPanic for Panicked {
    type ResponseBody = Body;

    /// The payload is deliberately unused: see the module documentation.
    fn response_for_panic(&mut self, _payload: Box<dyn Any + Send + 'static>) -> Response {
        error!(
            method = %self.method,
            route = %self.route,
            "an HTTP handler panicked",
        );

        (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            BODY,
        )
            .into_response()
    }
}
