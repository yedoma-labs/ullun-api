//! Background tasks and graceful shutdown

mod common;

use common::Server;
use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use ullun::prelude::*;

/// Start `app` with a manual stop button; the join handle yields once it has shut down
async fn spawn_stoppable(
    app: App,
) -> (
    Server,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<Result<()>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let handle = tokio::spawn(app.serve_with_shutdown(listener, async {
        let _ = stopped.await;
    }));
    (Server { addr }, stop, handle)
}

async fn finished(handle: tokio::task::JoinHandle<Result<()>>, within: Duration) {
    tokio::time::timeout(within, handle)
        .await
        .expect("server did not stop in time")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn in_flight_requests_finish_and_new_connections_are_refused() {
    let (started_tx, started_rx) = oneshot::channel::<()>();
    let started = Arc::new(tokio::sync::Mutex::new(Some(started_tx)));
    let app = App::new().get("/slow", move |_req: Request| {
        let started = started.clone();
        async move {
            if let Some(tx) = started.lock().await.take() {
                let _ = tx.send(());
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            Ok(Response::text("done"))
        }
    });
    let (server, stop, handle) = spawn_stoppable(app).await;

    let request = tokio::spawn(async move { server.get("/slow").send().await });
    started_rx.await.unwrap(); // the handler is running
    stop.send(()).unwrap();

    let res = request.await.unwrap();
    assert_eq!((res.status, res.text().as_str()), (200, "done"));
    finished(handle, Duration::from_secs(5)).await;

    assert!(
        TcpStream::connect(server.addr).await.is_err(),
        "listener should be closed"
    );
}

#[tokio::test]
async fn idle_keep_alive_connections_do_not_delay_shutdown() {
    let app = App::new().get("/", |_req: Request| async { Ok(Response::text("ok")) });
    let (server, stop, handle) = spawn_stoppable(app.shutdown_grace(Duration::from_secs(30))).await;

    // one finished request, connection kept open and idle
    let stream = TcpStream::connect(server.addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    let conn_task = tokio::spawn(conn);
    let res = sender
        .send_request(
            hyper::Request::builder()
                .uri("/")
                .header("host", "x")
                .body(Empty::<Bytes>::new())
                .unwrap(),
        )
        .await
        .unwrap();
    res.into_body().collect().await.unwrap();

    let t = Instant::now();
    stop.send(()).unwrap();
    finished(handle, Duration::from_secs(5)).await;
    assert!(t.elapsed() < Duration::from_secs(5));
    // the server hung up on the idle connection
    tokio::time::timeout(Duration::from_secs(5), conn_task)
        .await
        .expect("client connection still open")
        .unwrap()
        .ok();
}

#[tokio::test]
async fn background_tasks_run_use_state_and_stop_on_shutdown() {
    struct Counter(AtomicU32);
    let ended = Arc::new(AtomicBool::new(false));
    let ended_flag = ended.clone();

    let app = App::new()
        .state(Counter(AtomicU32::new(0)))
        .background(move |ctx: Context| async move {
            let counter = ctx.state::<Counter>().unwrap();
            // would run for a minute, but the sleep is cut short by shutdown
            while ctx.sleep(Duration::from_millis(10)).await {
                counter.0.fetch_add(1, Ordering::SeqCst);
            }
            // cleanup work still happens before the server reports "stopped"
            tokio::time::sleep(Duration::from_millis(50)).await;
            ended_flag.store(true, Ordering::SeqCst);
        })
        .get("/count", |req: Request| async move {
            Ok(Response::text(
                req.state::<Counter>()?.0.load(Ordering::SeqCst).to_string(),
            ))
        });
    let (server, stop, handle) = spawn_stoppable(app).await;

    tokio::time::sleep(Duration::from_millis(100)).await;
    let count: u32 = server.get("/count").send().await.text().parse().unwrap();
    assert!(
        count >= 2,
        "background task should have ticked, got {count}"
    );

    assert!(!ended.load(Ordering::SeqCst));
    stop.send(()).unwrap();
    finished(handle, Duration::from_secs(5)).await;
    assert!(
        ended.load(Ordering::SeqCst),
        "shutdown must wait for background tasks"
    );
}

#[tokio::test]
async fn long_lived_streams_end_promptly_when_they_watch_the_signal() {
    let app = App::new().get("/events", |req: Request| async move {
        let (tx, res) = Response::stream();
        let shutdown = req.shutdown_signal();
        tokio::spawn(async move {
            let _ = tx.send("hello\n").await;
            // an endless stream that still stops with the server
            shutdown.wait().await;
            let _ = tx.send("bye\n").await;
        });
        Ok(res.with_header("content-type", "text/plain"))
    });
    let (server, stop, handle) = spawn_stoppable(app.shutdown_grace(Duration::from_secs(30))).await;

    let stream = TcpStream::connect(server.addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let res = sender
        .send_request(
            hyper::Request::builder()
                .uri("/events")
                .header("host", "x")
                .body(Empty::<Bytes>::new())
                .unwrap(),
        )
        .await
        .unwrap();
    let mut body = res.into_body();
    assert_eq!(
        body.frame().await.unwrap().unwrap().into_data().unwrap(),
        "hello\n"
    );

    let t = Instant::now();
    stop.send(()).unwrap();
    assert_eq!(
        body.frame().await.unwrap().unwrap().into_data().unwrap(),
        "bye\n"
    );
    assert!(body.frame().await.is_none(), "stream should end");
    finished(handle, Duration::from_secs(5)).await;
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "must not wait for the 30 s grace period"
    );
}

#[tokio::test]
async fn handlers_that_ignore_shutdown_are_cut_off_after_the_grace_period() {
    let app = App::new().shutdown_grace(Duration::from_millis(200)).get(
        "/forever",
        |_req: Request| async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(Response::text("never"))
        },
    );
    let (server, stop, handle) = spawn_stoppable(app).await;

    let request = tokio::spawn(async move { server.get("/forever").send().await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let t = Instant::now();
    stop.send(()).unwrap();
    finished(handle, Duration::from_secs(5)).await;
    assert!(t.elapsed() < Duration::from_secs(3));
    request.abort();
}

#[tokio::test]
async fn a_panicking_background_task_does_not_take_the_server_down() {
    let app = App::new()
        .background(|_ctx: Context| async { panic!("boom") })
        .get("/", |_req: Request| async {
            Ok(Response::text("still here"))
        });
    let (server, stop, handle) = spawn_stoppable(app).await;

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(server.get("/").send().await.text(), "still here");
    stop.send(()).unwrap();
    finished(handle, Duration::from_secs(5)).await;
}

#[tokio::test]
async fn requests_outside_a_server_get_a_signal_that_never_fires() {
    let req = Request::new(
        http::Method::GET,
        "/".parse().unwrap(),
        http::HeaderMap::new(),
        bytes::Bytes::new(),
    );
    let signal = req.shutdown_signal();
    assert!(!signal.is_triggered());
    assert!(
        tokio::time::timeout(Duration::from_millis(30), signal.wait())
            .await
            .is_err()
    );
}
