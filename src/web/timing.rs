//! The render measurement: how long this half took to build one page.
//!
//! The target: a web page render within 500 ms at the 95th percentile, measured
//! server-side, excluding network transit, under normal load.
//!
//! "Excluding network transit" is what decides where the measurement lives: it
//! is taken around the layer's inner call, from the handler starting on a
//! request to the response body being built. This half's handlers render into
//! memory, so the future resolving is the body being built and nothing of the
//! write-back is inside it.
//!
//! Per route, not pooled: `/` lists games out of SQLite, `/games/{game_id}`
//! reads a snapshot out of memory or a row out of SQLite, and
//! `/games/{game_id}/record` reads a file. The route comes from [`MatchedPath`],
//! the template rather than the path, as `panics` reads it.
//!
//! [`enabled`] comes before the clock, so that with no subscriber collecting
//! `render_us` the layer does not read a clock, does not clone a route and does
//! not subtract; the callsite's `Interest` is cached, so what a request pays is
//! the load of a cached atomic.
//!
//! [`enabled`]: tracing::enabled

use std::time::Instant;

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use tracing::{Level, debug, event_enabled};

use super::matched_route;

/// Times one request and leaves the `render_us` record behind it.
///
/// Wrapped around the routes by [`router`](super::routes::router) inside the
/// catch-panic layer, so a panicked request contributes no sample rather than a
/// misleading one.
pub async fn measured(request: Request, next: Next) -> Response {
    // The clock is read only when something is listening for the field.
    let Some(started) = event_enabled!(Level::DEBUG, render_us).then(Instant::now) else {
        return next.run(request).await;
    };
    let route = matched_route(&request);

    let response = next.run(request).await;

    debug!(
        route = route.as_str(),
        status = response.status().as_u16(),
        render_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
        "page render",
    );

    response
}
