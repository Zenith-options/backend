use axum::body::Body;
use axum::extract::{MatchedPath, Request, State};
use axum::http::{header, HeaderValue};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

use crate::AppState;

const DURATION_BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

#[derive(Default)]
struct HttpSeries {
    count: u64,
    duration_sum: f64,
    buckets: [u64; DURATION_BUCKETS.len()],
}

#[derive(Default)]
pub struct Metrics {
    http: Mutex<BTreeMap<(String, String, String), HttpSeries>>,
    positions_opened: AtomicU64,
    positions_closed: AtomicU64,
    premium_volume: Mutex<f64>,
    collateral_locked: Mutex<f64>,
    alert_triggers: AtomicU64,
    rate_limit_rejections: AtomicU64,
    websocket_connections: AtomicU64,
    db_query_count: AtomicU64,
    db_query_duration_seconds: Mutex<f64>,
    channel_pool_size: AtomicU64,
    channel_pool_leased: AtomicU64,
    channel_pool_quarantined: AtomicU64,
    channel_lease_waits: AtomicU64,
    channel_lease_wait_duration_seconds: Mutex<f64>,
    channel_sequence_gap_recoveries: AtomicU64,
    channel_sequence_reservations: AtomicU64,
    rpc_retries: Mutex<BTreeMap<String, u64>>,
}

static GLOBAL_METRICS: OnceLock<Arc<Metrics>> = OnceLock::new();

pub fn global() -> Arc<Metrics> {
    GLOBAL_METRICS
        .get_or_init(|| Arc::new(Metrics::default()))
        .clone()
}

pub async fn request_metrics(request: Request<Body>, next: Next) -> Response {
    let method = request.method().as_str().to_owned();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|matched| matched.as_str().to_owned())
        .unwrap_or_else(|| "/_unmatched".to_owned());
    let started = Instant::now();
    let response = next.run(request).await;
    global().record_http(
        &method,
        &route,
        response.status().as_u16(),
        started.elapsed(),
    );
    response
}

impl Metrics {
    pub fn record_http(&self, method: &str, route: &str, status: u16, duration: Duration) {
        let status_class = format!("{}xx", status / 100);
        let seconds = duration.as_secs_f64();
        let mut http = self.http.lock().unwrap();
        let series = http
            .entry((method.to_owned(), route.to_owned(), status_class))
            .or_default();
        series.count += 1;
        series.duration_sum += seconds;
        for (index, bucket) in DURATION_BUCKETS.iter().enumerate() {
            if seconds <= *bucket {
                series.buckets[index] += 1;
            }
        }
        if status == 429 {
            self.rate_limit_rejections.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_rpc_retry(&self, reason: &str) {
        let mut retries = self.rpc_retries.lock().unwrap();
        *retries.entry(reason.to_owned()).or_insert(0) += 1;
    }

    pub fn record_database_query(&self, duration: Option<Duration>) {
        self.db_query_count.fetch_add(1, Ordering::Relaxed);
        if let Some(duration) = duration {
            *self.db_query_duration_seconds.lock().unwrap() += duration.as_secs_f64();
        }
    }

    pub fn record_position_opened(&self, premium: f64, collateral: f64) {
        self.positions_opened.fetch_add(1, Ordering::Relaxed);
        *self.premium_volume.lock().unwrap() += premium;
        *self.collateral_locked.lock().unwrap() += collateral;
    }

    pub fn record_position_closed(&self, premium: f64, collateral: f64) {
        self.positions_closed.fetch_add(1, Ordering::Relaxed);
        *self.premium_volume.lock().unwrap() += premium;
        let mut locked = self.collateral_locked.lock().unwrap();
        *locked = (*locked - collateral).max(0.0);
    }

    pub fn set_collateral_locked(&self, amount: f64) {
        *self.collateral_locked.lock().unwrap() = amount.max(0.0);
    }

    pub fn record_alert_triggers(&self, count: u64) {
        self.alert_triggers.fetch_add(count, Ordering::Relaxed);
    }

    pub fn set_channel_pool_size(&self, size: u64) {
        self.channel_pool_size.store(size, Ordering::Relaxed);
    }

    pub fn set_channel_pool_leased(&self, leased: u64) {
        self.channel_pool_leased.store(leased, Ordering::Relaxed);
    }

    pub fn set_channel_pool_quarantined(&self, quarantined: u64) {
        self.channel_pool_quarantined
            .store(quarantined, Ordering::Relaxed);
    }

    pub fn record_channel_lease_wait(&self, duration: Duration) {
        self.channel_lease_waits.fetch_add(1, Ordering::Relaxed);
        *self.channel_lease_wait_duration_seconds.lock().unwrap() += duration.as_secs_f64();
    }

    pub fn record_channel_sequence_gap_recovery(&self) {
        self.channel_sequence_gap_recoveries
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_channel_sequence_reservations(&self, count: u64) {
        self.channel_sequence_reservations
            .fetch_add(count, Ordering::Relaxed);
    }

    pub fn websocket_connected(self: &Arc<Self>) -> WebSocketConnection {
        self.websocket_connections.fetch_add(1, Ordering::Relaxed);
        WebSocketConnection(self.clone())
    }

    fn exposition(
        &self,
        pool_size: u32,
        pool_idle: usize,
        loop_health: impl Iterator<Item = (&'static str, bool)>,
    ) -> String {
        let mut output = String::from(
            "# HELP http_requests_total HTTP requests by route, method, and status class.\n\
             # TYPE http_requests_total counter\n\
             # HELP http_request_duration_seconds HTTP request duration in seconds.\n\
             # TYPE http_request_duration_seconds histogram\n",
        );
        let http = self.http.lock().unwrap();
        for ((method, route, status_class), series) in http.iter() {
            let labels = format!(
                "method=\"{}\",route=\"{}\",status_class=\"{}\"",
                escape_label(method),
                escape_label(route),
                escape_label(status_class)
            );
            output.push_str(&format!(
                "http_requests_total{{{labels}}} {}\n",
                series.count
            ));
            for (index, bucket) in DURATION_BUCKETS.iter().enumerate() {
                output.push_str(&format!(
                    "http_request_duration_seconds_bucket{{{labels},le=\"{bucket}\"}} {}\n",
                    series.buckets[index]
                ));
            }
            output.push_str(&format!(
                "http_request_duration_seconds_bucket{{{labels},le=\"+Inf\"}} {}\n\
                 http_request_duration_seconds_sum{{{labels}}} {}\n\
                 http_request_duration_seconds_count{{{labels}}} {}\n",
                series.count, series.duration_sum, series.count
            ));
        }
        drop(http);

        output.push_str(
            "# HELP zenith_rpc_retries_total Stellar RPC client retry attempts by reason.\n\
             # TYPE zenith_rpc_retries_total counter\n",
        );
        let rpc_retries = self.rpc_retries.lock().unwrap();
        for (reason, count) in rpc_retries.iter() {
            output.push_str(&format!(
                "zenith_rpc_retries_total{{reason=\"{}\"}} {}\n",
                escape_label(reason),
                count
            ));
        }
        drop(rpc_retries);

        output.push_str(
            "# HELP zenith_db_pool_connections Current SQLite pool connections.\n\
             # TYPE zenith_db_pool_connections gauge\n",
        );
        output.push_str(&format!("zenith_db_pool_connections {pool_size}\n"));
        output.push_str(
            "# HELP zenith_db_pool_idle_connections Current idle SQLite pool connections.\n\
             # TYPE zenith_db_pool_idle_connections gauge\n",
        );
        output.push_str(&format!("zenith_db_pool_idle_connections {pool_idle}\n"));
        output.push_str(
            "# HELP zenith_db_queries_total Completed SQLx query spans.\n\
             # TYPE zenith_db_queries_total counter\n",
        );
        output.push_str(&format!(
            "zenith_db_queries_total {}\n",
            self.db_query_count.load(Ordering::Relaxed)
        ));
        output.push_str(
            "# HELP zenith_db_query_duration_seconds_sum Total completed SQLx query duration.\n\
             # TYPE zenith_db_query_duration_seconds_sum counter\n",
        );
        output.push_str(&format!(
            "zenith_db_query_duration_seconds_sum {}\n",
            *self.db_query_duration_seconds.lock().unwrap()
        ));
        output
    }
}

pub struct WebSocketConnection(Arc<Metrics>);

impl Drop for WebSocketConnection {
    fn drop(&mut self) {
        self.0.websocket_connections.fetch_sub(1, Ordering::Relaxed);
    }
}

fn escape_label(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
