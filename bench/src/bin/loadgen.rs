//! `loadgen http://127.0.0.1:3000/path [-c 64] [-d 10] [-w 2] [-t 4] [-m POST] [-b BODY] [-H "k: v"]...`
//!
//! Prints one JSON object (requests, errors, rps, latency percentiles in
//! microseconds) on stdout and a short summary on stderr.

use std::time::Duration;
use ullun_bench::load::{run, LoadOpts};

fn main() {
    let mut args = std::env::args().skip(1);
    let mut url = None;
    let mut opts = LoadOpts {
        addr: ([127, 0, 0, 1], 80).into(),
        method: "GET".into(),
        path: "/".into(),
        body: String::new(),
        headers: Vec::new(),
        connections: 64,
        duration: Duration::from_secs(10),
        warmup: Duration::from_secs(2),
        threads: (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) / 2).max(1),
    };
    while let Some(a) = args.next() {
        let mut next = |what: &str| args.next().unwrap_or_else(|| usage(&format!("{what} needs a value")));
        match a.as_str() {
            "-c" | "--connections" => opts.connections = next("-c").parse().unwrap_or_else(|_| usage("bad -c")),
            "-d" | "--duration" => opts.duration = Duration::from_secs_f64(next("-d").parse().unwrap_or_else(|_| usage("bad -d"))),
            "-w" | "--warmup" => opts.warmup = Duration::from_secs_f64(next("-w").parse().unwrap_or_else(|_| usage("bad -w"))),
            "-t" | "--threads" => opts.threads = next("-t").parse().unwrap_or_else(|_| usage("bad -t")),
            "-m" | "--method" => opts.method = next("-m"),
            "-b" | "--body" => opts.body = next("-b"),
            "-H" | "--header" => {
                let h = next("-H");
                let (k, v) = h.split_once(':').unwrap_or_else(|| usage("header must look like 'name: value'"));
                opts.headers.push((k.trim().to_string(), v.trim().to_string()));
            }
            u if !u.starts_with('-') => url = Some(u.to_string()),
            other => usage(&format!("unknown option {other}")),
        }
    }
    let url = url.unwrap_or_else(|| usage("missing URL"));
    let rest = url.strip_prefix("http://").unwrap_or_else(|| usage("only http:// URLs are supported"));
    let (authority, path) = rest.split_once('/').map(|(a, p)| (a, format!("/{p}"))).unwrap_or((rest, "/".into()));
    opts.addr = std::net::ToSocketAddrs::to_socket_addrs(authority)
        .ok()
        .and_then(|mut a| a.next())
        .unwrap_or_else(|| usage("cannot resolve host"));
    opts.path = path;

    let r = run(&opts);
    eprintln!(
        "{:.0} req/s  p50 {} us  p99 {} us  max {} us  ({} requests, {} errors in {:.1}s)",
        r.rps, r.latency_us.p50, r.latency_us.p99, r.latency_us.max, r.requests, r.errors, r.duration_s
    );
    println!("{}", serde_json::to_string(&r).expect("json"));
}

fn usage(why: &str) -> ! {
    eprintln!("{why}\nusage: loadgen URL [-c connections] [-d seconds] [-w warmup seconds] [-t threads] [-m METHOD] [-b BODY] [-H 'name: value']");
    std::process::exit(2);
}
