#[tokio::main]
async fn main() {
    zenith_backend::init_tracing();
    let state = zenith_backend::init_state().await;
    let app = zenith_backend::build_router(state);

    let addr = "0.0.0.0:8081";
    println!("Zenith backend listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(zenith_backend::shutdown_signal())
        .await
        .unwrap();
}
