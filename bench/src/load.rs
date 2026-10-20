//! Closed-loop HTTP/1.1 load generator: N keep-alive connections, each sends the
//! next request when the previous response is complete.

use crate::{Latency, LoadResult};
use bytes::Bytes;
use hdrhistogram::Histogram;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;

#[derive(Clone)]
pub struct LoadOpts {
    pub addr: SocketAddr,
    pub method: String,
    pub path: String,
    pub body: String,
    pub headers: Vec<(String, String)>,
    pub connections: usize,
    pub duration: Duration,
    pub warmup: Duration,
    pub threads: usize,
}

struct Phase {
    hist: Histogram<u64>,
    requests: u64,
    errors: u64,
    bytes: u64,
}

async fn connection(opts: &LoadOpts, deadline: Instant) -> Phase {
    let mut phase = Phase { hist: Histogram::new_with_bounds(1, 60_000_000, 3).expect("histogram"), requests: 0, errors: 0, bytes: 0 };
    let body = Bytes::from(opts.body.clone());
    let mut sender: Option<hyper::client::conn::http1::SendRequest<Full<Bytes>>> = None;

    while Instant::now() < deadline {
        if sender.is_none() {
            match TcpStream::connect(opts.addr).await {
                Ok(stream) => {
                    let _ = stream.set_nodelay(true);
                    match hyper::client::conn::http1::handshake(TokioIo::new(stream)).await {
                        Ok((s, conn)) => {
                            tokio::spawn(conn);
                            sender = Some(s);
                        }
                        Err(_) => {
                            phase.errors += 1;
                            tokio::time::sleep(Duration::from_millis(1)).await;
                            continue;
                        }
                    }
                }
                Err(_) => {
                    phase.errors += 1;
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    continue;
                }
            }
        }
        let s = sender.as_mut().expect("connected");
        let mut req = hyper::Request::builder()
            .method(opts.method.as_str())
            .uri(opts.path.as_str())
            .header("host", opts.addr.to_string());
        for (k, v) in &opts.headers {
            req = req.header(k, v);
        }
        if !body.is_empty() && !opts.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-type")) {
            req = req.header("content-type", "application/json");
        }
        let req = req.body(Full::new(body.clone())).expect("request");

        let started = Instant::now();
        if s.ready().await.is_err() {
            sender = None;
            phase.errors += 1;
            continue;
        }
        match s.send_request(req).await {
            Ok(res) => {
                let ok = res.status().is_success();
                let closes = res.headers().get("connection").is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"close"));
                match res.into_body().collect().await {
                    Ok(collected) if ok => {
                        let micros = started.elapsed().as_micros().clamp(1, 60_000_000) as u64;
                        let _ = phase.hist.record(micros);
                        phase.requests += 1;
                        phase.bytes += collected.to_bytes().len() as u64;
                    }
                    _ => phase.errors += 1,
                }
                if closes {
                    sender = None;
                }
            }
            Err(_) => {
                phase.errors += 1;
                sender = None;
            }
        }
    }
    phase
}

async fn run_phase(opts: &LoadOpts, duration: Duration) -> (Phase, f64) {
    let started = Instant::now();
    let deadline = started + duration;
    let mut tasks = Vec::new();
    for _ in 0..opts.connections {
        let opts = opts.clone();
        tasks.push(tokio::spawn(async move { connection(&opts, deadline).await }));
    }
    let mut total = Phase { hist: Histogram::new_with_bounds(1, 60_000_000, 3).expect("histogram"), requests: 0, errors: 0, bytes: 0 };
    for t in tasks {
        if let Ok(p) = t.await {
            total.hist.add(&p.hist).ok();
            total.requests += p.requests;
            total.errors += p.errors;
            total.bytes += p.bytes;
        }
    }
    (total, started.elapsed().as_secs_f64())
}

/// Warm up, then measure. Blocks the calling thread.
pub fn run(opts: &LoadOpts) -> LoadResult {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(opts.threads.max(1))
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        if !opts.warmup.is_zero() {
            let _ = run_phase(opts, opts.warmup).await;
        }
        let (phase, secs) = run_phase(opts, opts.duration).await;
        LoadResult {
            requests: phase.requests,
            errors: phase.errors,
            duration_s: secs,
            rps: phase.requests as f64 / secs,
            bytes: phase.bytes,
            latency_us: Latency {
                mean: phase.hist.mean(),
                p50: phase.hist.value_at_quantile(0.50),
                p90: phase.hist.value_at_quantile(0.90),
                p99: phase.hist.value_at_quantile(0.99),
                p999: phase.hist.value_at_quantile(0.999),
                max: phase.hist.max(),
            },
        }
    })
}
