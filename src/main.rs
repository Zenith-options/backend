#[tokio::main]
async fn main() {
    zenith_backend::init_tracing();
    let state = zenith_backend::init_state().await;
    let shutdown_state = state.clone();
    let app = zenith_backend::build_router(state);

    let addr = "0.0.0.0:8081";
    tracing::info!(address = addr, "service listening");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    // Needed for SmartIpKeyExtractor's peer-IP fallback (used when no
    // x-forwarded-for/x-real-ip/forwarded header is present) to have a
    // real socket address to read, rather than nothing at all.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(zenith_backend::shutdown_signal(shutdown_state))
    .await
    .unwrap();
    if let Err(error) = zenith_backend::telemetry::shutdown() {
        tracing::error!(error = %error, "failed to flush OpenTelemetry traces");
    }
}
