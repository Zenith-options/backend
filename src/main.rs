#[tokio::main]
async fn main() {
    zenith_backend::init_tracing();
    dotenvy::dotenv().ok();
    let config = zenith_backend::config::Config::load().expect("invalid application configuration");
    let addr = config.bind_address.clone();
    let state = zenith_backend::init_state_with_config(config).await;
    let app = zenith_backend::build_router(state);

    println!("Zenith backend listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    // Needed for SmartIpKeyExtractor's peer-IP fallback (used when no
    // x-forwarded-for/x-real-ip/forwarded header is present) to have a
    // real socket address to read, rather than nothing at all.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(zenith_backend::shutdown_signal())
    .await
    .unwrap();
}
