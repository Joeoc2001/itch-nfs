//! A stand-in for the itch.io API serving a fixture pack, so that CI can run
//! the real server binary and mount it without an itch.io account.
//!
//!     cargo run --example mock_itch -- 127.0.0.1:8099
//!     ITCH_TOKEN=test-token itch-nfs --api-url http://127.0.0.1:8099

#[path = "../tests/support/mod.rs"]
mod support;

use std::sync::Arc;

#[tokio::main]
async fn main() {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:8099".into());
    let mock = Arc::new(support::Mock::default());
    *mock.uploads.lock().unwrap() = support::fixture_uploads();
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    eprintln!(
        "mock itch.io API on http://{addr} (token: {})",
        support::TOKEN
    );
    axum::serve(listener, support::router(mock)).await.unwrap();
}
