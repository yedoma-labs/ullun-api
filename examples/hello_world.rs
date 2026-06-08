//! Simplest possible ullun-api server

use ullun::prelude::*;

#[tokio::main]
async fn main() {
    App::new()
        .get("/", |_req: Request| async {
            Ok(Response::text("Hello, World!"))
        })
        .get_with_params("/hello/{name}", |params: Params| async move {
            let name = params.get("name")?;
            Ok(Response::json(serde_json::json!({
                "message": format!("Hello, {}!", name)
            })))
        })
        .run("127.0.0.1:3000")
        .await
        .unwrap();
}
