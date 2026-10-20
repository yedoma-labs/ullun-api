//! Chat server with rooms, history and presence (features `websocket` and `jwt`)
//!
//! Browsers log in (password -> bearer token), trade the token for a one-time
//! WebSocket ticket (so no long-lived credential ever appears in a URL), and
//! connect to a room. Messages fan out through a `Hub`, late joiners receive the
//! recent history, presence is announced, senders are rate limited, dead
//! connections are pinged out, and the server closes sockets politely on shutdown.
//!
//! Configuration (environment):
//!   PORT         listen port (default 3000)
//!   CHAT_USERS   `name:password,name:password` (required unless DEV=1)
//!   JWT_SECRET   at least 32 bytes (required unless DEV=1)
//!   DEV          1 = users alice/bob (password "demo-password"), demo secret
//!
//! Run:   DEV=1 cargo run --example chat --features websocket,jwt   then open http://localhost:3000

mod app;

#[tokio::main]
async fn main() {
    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".into());
    let config = match app::Config::from_env() {
        Ok(config) => config,
        Err(message) => {
            eprintln!("configuration error: {message}");
            std::process::exit(2);
        }
    };
    app::build(config)
        .run(&format!("0.0.0.0:{port}"))
        .await
        .unwrap();
}
