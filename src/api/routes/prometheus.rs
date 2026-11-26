use axum::{extract::State, response::IntoResponse, Extension};
use crate::api::app::AppState;
use crate::api::readiness::Readiness;

/// `GET /metrics`, Prometheus text format.
///
/// The replica's own load gauges are always exported, with or without the
/// `prometheus` feature: an autoscaler needs a per-pod concurrency signal,
/// and CPU says little about a proxy that mostly waits on upstreams. The
/// feature adds the request, token, cost and policy counters in front.
#[cfg_attr(not(feature = "prometheus"), allow(unused_variables))]
pub async fn metrics_handler(
    State(state): State<AppState>,
    Extension(readiness): Extension<Readiness>,
) -> impl IntoResponse {
    #[allow(unused_mut)]
    let mut buf: Vec<u8> = Vec::new();
    #[cfg(feature = "prometheus")]
    {
        use ::prometheus::Encoder;
        if let Some(ref metrics) = state.app_metrics {
            let encoder = ::prometheus::TextEncoder::new();
            let families = metrics.registry.gather();
            encoder.encode(&families, &mut buf).unwrap_or_default();
        }
    }
    buf.extend_from_slice(replica_gauges(&readiness).as_bytes());
    (
        axum::http::StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        buf,
    )
        .into_response()
}

/// The gauges read from the drain accounting. `requests_in_flight` counts a
/// stream until its last byte is written; probes and scrapes are not counted.
pub fn replica_gauges(readiness: &Readiness) -> String {
    format!(
        "# HELP modelrouter_requests_in_flight Requests this replica is serving now; a stream counts until its last byte.\n\
         # TYPE modelrouter_requests_in_flight gauge\n\
         modelrouter_requests_in_flight {}\n\
         # HELP modelrouter_draining 1 while this replica drains after SIGTERM, else 0.\n\
         # TYPE modelrouter_draining gauge\n\
         modelrouter_draining {}\n",
        readiness.in_flight(),
        u8::from(readiness.is_draining()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(text: &str, name: &str) -> String {
        text.lines()
            .find(|l| l.starts_with(&format!("{name} ")))
            .unwrap_or_else(|| panic!("{name} missing from {text}"))
            .rsplit(' ')
            .next()
            .unwrap()
            .to_string()
    }

    #[test]
    fn gauges_follow_in_flight_guards_and_drain() {
        let r = Readiness::started();
        let text = replica_gauges(&r);
        assert_eq!(value(&text, "modelrouter_requests_in_flight"), "0");
        assert_eq!(value(&text, "modelrouter_draining"), "0");
        assert!(text.contains("# TYPE modelrouter_requests_in_flight gauge"));

        let a = r.track().unwrap();
        let _b = r.track().unwrap();
        assert_eq!(value(&replica_gauges(&r), "modelrouter_requests_in_flight"), "2");
        drop(a);
        assert_eq!(value(&replica_gauges(&r), "modelrouter_requests_in_flight"), "1");

        r.begin_drain();
        assert_eq!(value(&replica_gauges(&r), "modelrouter_draining"), "1");
    }
}
