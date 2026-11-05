//! Drain gate: count requests in flight and refuse new ones while draining.
//!
//! Sits outermost on the router. Health and metrics paths pass through
//! uncounted so the platform can still read readiness (503 while draining)
//! and liveness (200) from a pod that is on its way out. Every other
//! request holds an in-flight guard until its response body is fully
//! written, so a long stream counts until its last byte.

use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http_body::{Frame, SizeHint};

use crate::api::readiness::{InFlightGuard, Readiness};

/// Paths a draining replica still answers: the probes, and the scrape.
fn exempt(path: &str) -> bool {
    path == "/health" || path.starts_with("/health/") || path == "/metrics"
}

pub async fn drain_gate(
    State(readiness): State<Readiness>,
    request: Request,
    next: Next,
) -> Response {
    if exempt(request.uri().path()) {
        return next.run(request).await;
    }
    let Some(guard) = readiness.track() else {
        return refused();
    };
    let response = next.run(request).await;
    response.map(|body| {
        Body::new(Guarded {
            inner: Box::pin(body),
            _guard: guard,
        })
    })
}

fn refused() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CONNECTION, "close")],
        axum::Json(serde_json::json!({
            "error": {"message": "server is shutting down; retry on another replica", "type": "shutting_down"}
        })),
    )
        .into_response()
}

/// A response body that releases its in-flight guard when it is dropped,
/// which hyper does after the last frame is written or the client goes away.
struct Guarded {
    inner: Pin<Box<Body>>,
    _guard: InFlightGuard,
}

impl http_body::Body for Guarded {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.inner.as_mut().poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    fn app(readiness: &Readiness) -> Router {
        Router::new()
            .route("/v1/models", get(|| async { "ok" }))
            .route("/health/live", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                readiness.clone(),
                drain_gate,
            ))
    }

    async fn call(app: Router, path: &str) -> Response {
        app.oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_request_counts_until_its_body_is_consumed() {
        let readiness = Readiness::started();
        let response = call(app(&readiness), "/v1/models").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(readiness.in_flight(), 1, "body not yet read");
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"ok");
        assert_eq!(readiness.in_flight(), 0);
    }

    #[tokio::test]
    async fn draining_refuses_with_503_and_connection_close() {
        let readiness = Readiness::started();
        readiness.begin_drain();
        let response = call(app(&readiness), "/v1/models").await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::CONNECTION], "close");
        assert_eq!(readiness.refused(), 1);
        assert_eq!(readiness.in_flight(), 0);
    }

    #[tokio::test]
    async fn health_paths_pass_uncounted_while_draining() {
        let readiness = Readiness::started();
        readiness.begin_drain();
        let response = call(app(&readiness), "/health/live").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(readiness.refused(), 0);
        assert_eq!(readiness.in_flight(), 0);
    }
}
