//! Blog / small CMS
//!
//! Public blog (index with search and tags, posts rendered from Markdown, RSS
//! feed, sitemap) and an admin area with login, drafts, an editor and
//! publishing. Server-side sessions, CSRF protection on every form, hashed
//! password, login throttling, strict CSP and escaped Markdown (raw HTML in
//! posts is shown as text, `javascript:` links are neutralised).
//!
//! Configuration (environment):
//!   PORT                 listen port (default 3000)
//!   BASE_URL             public URL for feed and sitemap (default http://localhost:PORT)
//!   SITE_TITLE           default "My blog"
//!   ADMIN_USER           admin name (default "admin")
//!   ADMIN_PASSWORD_HASH  argon2 PHC hash of the admin password (required unless DEV=1)
//!   DATA_FILE            JSON file for posts (default: in memory)
//!   DEV                  1 = password "dev-password", cookies without Secure (plain http)
//!
//! Run:   DEV=1 cargo run --example blog        then open http://localhost:3000 and /admin
//! Hash:  cargo run --example blog -- hash-password 'my secret'

mod app;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("hash-password") {
        match args.get(1) {
            Some(password) => println!("{}", app::hash_password(password)),
            None => eprintln!("usage: blog hash-password <password>"),
        }
        return;
    }

    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".into());
    let config = match app::Config::from_env(&port) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("configuration error: {message}");
            std::process::exit(2);
        }
    };
    if config.dev {
        eprintln!("DEV mode: admin password is \"dev-password\", cookies are not Secure");
    }
    app::build(config)
        .run(&format!("0.0.0.0:{port}"))
        .await
        .unwrap();
}
