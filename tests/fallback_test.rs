//! Custom fallback handler and panic isolation

mod common;

use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use ullun::prelude::*;
use ullun::testing::TestClient;

fn app() -> App {
    App::new()
        .middleware(|req: Request, next: Next| async move {
            // sees errors as responses, like the built-in middleware does
            let res = next.run(req).await.unwrap_or_else(Response::from_error);
            Ok(res.with_header("x-mw", "1"))
        })
        .get("/known", |_req: Request| async {
            Ok(Response::text("known"))
        })
        .get("/panic", |_req: Request| async {
            if true {
                panic!("handler blew up: secret detail");
            }
            Ok(Response::text("unreachable"))
        })
        .get("/panic-late", |_req: Request| async {
            tokio::task::yield_now().await;
            if true {
                panic!("after an await");
            }
            Ok(Response::text("unreachable"))
        })
        .group("/g", |g| {
            g.middleware(|req: Request, next: Next| async move {
                Ok(next.run(req).await?.with_header("x-group", "1"))
            })
            .get("/panic", |_req: Request| async {
                if true {
                    panic!("in a group");
                }
                Ok(Response::text("unreachable"))
            })
        })
}

#[tokio::test]
async fn fallback_answers_unknown_paths_inside_the_middleware_chain() {
    let c = TestClient::new(app().fallback(|req: Request| async move {
        Ok(
            Response::html(format!("<h1>nothing at {}</h1>", req.uri.path()))
                .with_status(http::StatusCode::NOT_FOUND),
        )
    }));
    let res = c.get("/missing/page").send().await;
    assert_eq!(res.status(), 404);
    assert_eq!(res.text(), "<h1>nothing at /missing/page</h1>");
    assert_eq!(res.header("x-mw"), "1");
    // every method falls back for an unknown path
    assert_eq!(
        c.post("/missing").send().await.text(),
        "<h1>nothing at /missing</h1>"
    );

    // known paths keep their routes, and a wrong method is still 405 with Allow
    assert_eq!(c.get("/known").send().await.text(), "known");
    let wrong = c.delete("/known").send().await;
    assert_eq!(wrong.status(), 405);
    assert!(wrong.header("allow").contains("GET"));
}

#[tokio::test]
async fn fallback_can_serve_a_single_page_app() {
    let c = TestClient::new(app().fallback(|_req: Request| async { Ok(Response::html("<app/>")) }));
    let res = c.get("/orders/17").send().await;
    assert_eq!((res.status(), res.text().as_str()), (200, "<app/>"));
}

#[tokio::test]
async fn without_a_fallback_the_default_404_stays() {
    let c = TestClient::new(app());
    let res = c.get("/nope").send().await;
    assert_eq!(res.status(), 404);
    assert!(res.text().contains("Route not found"));
}

#[tokio::test]
async fn a_panicking_handler_becomes_a_500_that_middleware_sees() {
    let c = TestClient::new(app());
    for path in ["/panic", "/panic-late", "/g/panic"] {
        let res = c.get(path).send().await;
        assert_eq!(res.status(), 500, "{path}");
        // nothing internal leaks to the client
        assert!(!res.text().contains("secret detail"));
        assert!(res.text().contains("Internal server error"));
        assert_eq!(res.header("x-mw"), "1");
    }
    // the app keeps serving
    assert_eq!(c.get("/known").send().await.text(), "known");
}

#[tokio::test]
async fn the_connection_survives_a_panic_on_a_real_server() {
    let s = common::spawn(app()).await;
    let stream = tokio::net::TcpStream::connect(s.addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    for (path, status) in [
        ("/panic", 500),
        ("/known", 200),
        ("/panic-late", 500),
        ("/known", 200),
    ] {
        let req = hyper::Request::builder()
            .uri(path)
            .header("host", "x")
            .body(Empty::<Bytes>::new())
            .unwrap();
        let res = sender
            .send_request(req)
            .await
            .expect("same connection must keep working");
        assert_eq!(res.status(), status, "{path}");
        let _ = res.into_body().collect().await;
    }
}

#[tokio::test]
async fn panic_catching_can_be_turned_off() {
    // the panic then unwinds out of the pipeline; the test client surfaces it as a panic
    let c = TestClient::new(app().catch_panics(false));
    let task = tokio::spawn(async move { c.get("/panic").send().await });
    assert!(task.await.unwrap_err().is_panic());
}
