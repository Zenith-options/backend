#[tokio::main]
async fn main() {
    zenith_backend::init_tracing();
    dotenvy::dotenv().ok();
    let state = zenith_backend::init_state().await;
    let app = zenith_backend::build_router(state);

    let addr = std::env::var("ZENITH_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8081".to_string());
    println!("Zenith backend listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();

    // Track in-flight requests so the shutdown path can report how many
    // requests were still being served when SIGTERM arrived.
    let in_flight = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let in_flight_for_layer = in_flight.clone();
    let app = app.layer(axum::middleware::from_fn(
        move |req: axum::extract::Request, next: axum::middleware::Next| {
            let in_flight = in_flight_for_layer.clone();
            async move {
                in_flight.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let response = next.run(req).await;
                in_flight.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                response
            }
        },
    ));

    let in_flight_for_shutdown = in_flight.clone();
    let shutdown_started = std::time::Instant::now();
    // Needed for SmartIpKeyExtractor's peer-IP fallback (used when no
    // x-forwarded-for/x-real-ip/forwarded header is present) to have a
    // real socket address to read, rather than nothing at all.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        zenith_backend::shutdown_signal().await;
        let count = in_flight_for_shutdown.load(std::sync::atomic::Ordering::SeqCst);
        tracing::info!(in_flight_requests = count, "shutdown started");
    })
    .await
    .unwrap();

    let drain_duration_ms = shutdown_started.elapsed().as_millis() as u64;
    tracing::info!(drain_duration_ms, "shutdown complete");
}
