//! Runs every framework through every scenario and reports a table.
//!
//! `cargo run --release --bin bench-run -- [options]` (or `scripts/bench.sh`)
//!
//!   --duration SECS       measured time per run          (default 8)
//!   --warmup SECS         warm-up before measuring       (default 2)
//!   --rounds N            repetitions, median is used    (default 3)
//!   --connections N       keep-alive connections         (default 64)
//!   --server-threads N    worker threads of each server  (default: half of the cores)
//!   --load-threads N      threads of the load generator  (default: the other half)
//!   --frameworks a,b      subset of ullun,axum,actix
//!   --scenarios a,b       subset of hello,json,param,post,middleware
//!   --out DIR             where to write results         (default ./results)
//!   --quick               1 s runs, 1 round: smoke test of the tooling

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use ullun_bench::load::{run, LoadOpts};
use ullun_bench::{median, request_for, LoadResult, FRAMEWORKS, SCENARIOS};

struct Opts {
    duration: f64,
    warmup: f64,
    rounds: usize,
    connections: usize,
    server_threads: usize,
    load_threads: usize,
    frameworks: Vec<String>,
    scenarios: Vec<String>,
    out: PathBuf,
}

fn parse() -> Opts {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let mut o = Opts {
        duration: 8.0,
        warmup: 2.0,
        rounds: 3,
        connections: 64,
        server_threads: (cores / 2).max(1),
        load_threads: (cores - cores / 2).max(1),
        frameworks: FRAMEWORKS.iter().map(|s| s.to_string()).collect(),
        scenarios: SCENARIOS.iter().map(|s| s.to_string()).collect(),
        out: PathBuf::from("results"),
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = |n: &str| args.next().unwrap_or_else(|| die(&format!("{n} needs a value")));
        let num = |v: String, n: &str| -> f64 { v.parse().unwrap_or_else(|_| die(&format!("bad value for {n}"))) };
        match a.as_str() {
            "--duration" => o.duration = num(val("--duration"), "--duration"),
            "--warmup" => o.warmup = num(val("--warmup"), "--warmup"),
            "--rounds" => o.rounds = num(val("--rounds"), "--rounds") as usize,
            "--connections" => o.connections = num(val("--connections"), "--connections") as usize,
            "--server-threads" => o.server_threads = num(val("--server-threads"), "--server-threads") as usize,
            "--load-threads" => o.load_threads = num(val("--load-threads"), "--load-threads") as usize,
            "--frameworks" => o.frameworks = val("--frameworks").split(',').map(str::to_string).collect(),
            "--scenarios" => o.scenarios = val("--scenarios").split(',').map(str::to_string).collect(),
            "--out" => o.out = PathBuf::from(val("--out")),
            "--quick" => {
                o.duration = 1.0;
                o.warmup = 0.3;
                o.rounds = 1;
                o.connections = 16;
            }
            "-h" | "--help" => die(""),
            other => die(&format!("unknown option {other}")),
        }
    }
    for f in &o.frameworks {
        if !FRAMEWORKS.contains(&f.as_str()) {
            die(&format!("unknown framework {f}"));
        }
    }
    for s in &o.scenarios {
        if !SCENARIOS.contains(&s.as_str()) {
            die(&format!("unknown scenario {s}"));
        }
    }
    o
}

fn die(why: &str) -> ! {
    if !why.is_empty() {
        eprintln!("{why}");
    }
    eprintln!("see the header of bench/src/bin/bench-run.rs for the options");
    std::process::exit(2);
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).map(|a| a.port()).expect("free port")
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())
}

fn cpu_model() -> String {
    if let Some(m) = command_output("sysctl", &["-n", "machdep.cpu.brand_string"]) {
        return m;
    }
    if let Ok(info) = std::fs::read_to_string("/proc/cpuinfo") {
        if let Some(line) = info.lines().find(|l| l.starts_with("model name")) {
            return line.split(':').nth(1).unwrap_or("").trim().to_string();
        }
    }
    std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "unknown".into())
}

/// Resident memory of a process in KiB
fn rss_kib(pid: u32) -> Option<u64> {
    if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
        return status.lines().find(|l| l.starts_with("VmRSS:"))?.split_whitespace().nth(1)?.parse().ok();
    }
    command_output("ps", &["-o", "rss=", "-p", &pid.to_string()])?.trim().parse().ok()
}

fn lock_version(name: &str) -> String {
    let lock = std::fs::read_to_string("Cargo.lock").unwrap_or_default();
    let mut lines = lock.lines();
    while let Some(l) = lines.next() {
        if l == format!("name = \"{name}\"") {
            if let Some(v) = lines.next() {
                return v.trim_start_matches("version = ").trim_matches('"').to_string();
            }
        }
    }
    "?".into()
}

fn server_path() -> PathBuf {
    let exe = std::env::current_exe().expect("current exe");
    exe.with_file_name(if cfg!(windows) { "bench-server.exe" } else { "bench-server" })
}

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_server(framework: &str, scenario: &str, port: u16, threads: usize) -> Server {
    let child = Command::new(server_path())
        .args([framework, scenario, &port.to_string(), &threads.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| die(&format!("cannot start {}: {e} (build with `cargo build --release` first)", server_path().display())));
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let deadline = Instant::now() + Duration::from_secs(20);
    while TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err() {
        if Instant::now() > deadline {
            die(&format!("{framework}/{scenario} did not start listening on {port}"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Server(child)
}

struct Measured {
    result: LoadResult,
    rss_kib: Option<u64>,
}

fn measure(o: &Opts, framework: &str, scenario: &str) -> Measured {
    let port = free_port();
    let server = start_server(framework, scenario, port, o.server_threads);
    let (method, path, body) = request_for(scenario);
    let result = run(&LoadOpts {
        addr: ([127, 0, 0, 1], port).into(),
        method: method.into(),
        path: path.into(),
        body: body.into(),
        headers: Vec::new(),
        connections: o.connections,
        duration: Duration::from_secs_f64(o.duration),
        warmup: Duration::from_secs_f64(o.warmup),
        threads: o.load_threads,
    });
    let rss = rss_kib(server.0.id());
    Measured { result, rss_kib: rss }
}

fn main() {
    let o = parse();
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0);
    let started = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);

    let machine = serde_json::json!({
        "os": format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        "cpu": cpu_model(),
        "logical_cores": cores,
        "rustc": command_output("rustc", &["--version"]),
        "ullun_api": lock_version("ullun-api"),
        "axum": lock_version("axum"),
        "actix_web": lock_version("actix-web"),
        "git": command_output("git", &["rev-parse", "--short", "HEAD"]),
        "unix_time": started,
    });
    eprintln!("machine: {}", machine);
    eprintln!(
        "{} connections, {} s measured (+{} s warm-up), {} rounds, {} server / {} load-generator threads on {} logical cores\n",
        o.connections, o.duration, o.warmup, o.rounds, o.server_threads, o.load_threads, cores
    );
    if o.server_threads + o.load_threads > cores {
        eprintln!("note: more threads than cores, results will be noisy\n");
    }

    // scenario -> framework -> best-of-median round
    let mut results: BTreeMap<String, BTreeMap<String, serde_json::Value>> = BTreeMap::new();
    for scenario in &o.scenarios {
        for framework in &o.frameworks {
            let mut rounds: Vec<Measured> = (0..o.rounds)
                .map(|i| {
                    eprint!("{scenario:>10} / {framework:<6} round {}/{} ... ", i + 1, o.rounds);
                    let m = measure(&o, framework, scenario);
                    eprintln!("{:.0} req/s (p99 {} us, {} errors)", m.result.rps, m.result.latency_us.p99, m.result.errors);
                    m
                })
                .collect();
            let med = median(rounds.iter().map(|m| m.result.rps).collect());
            // the round closest to the median represents the run
            rounds.sort_by(|a, b| (a.result.rps - med).abs().partial_cmp(&(b.result.rps - med).abs()).unwrap_or(std::cmp::Ordering::Equal));
            let rep = &rounds[0];
            let errors: u64 = rounds.iter().map(|m| m.result.errors).sum();
            results.entry(scenario.clone()).or_default().insert(
                framework.clone(),
                serde_json::json!({
                    "rps_median": med,
                    "rps_all": rounds.iter().map(|m| m.result.rps).collect::<Vec<_>>(),
                    "latency_us": rep.result.latency_us,
                    "errors": errors,
                    "rss_mib": rep.rss_kib.map(|k| k as f64 / 1024.0),
                }),
            );
        }
    }

    // Markdown report
    let mut md = String::new();
    md.push_str(&format!(
        "### {} - {} logical cores - {}\n\n",
        machine["cpu"].as_str().unwrap_or("?"),
        cores,
        machine["os"].as_str().unwrap_or("?")
    ));
    md.push_str(&format!(
        "ullun-api {}, axum {}, actix-web {}, {}; {} connections, {} s x {} rounds (median), {} server / {} load threads\n\n",
        machine["ullun_api"].as_str().unwrap_or("?"),
        machine["axum"].as_str().unwrap_or("?"),
        machine["actix_web"].as_str().unwrap_or("?"),
        machine["rustc"].as_str().unwrap_or("rustc ?"),
        o.connections, o.duration, o.rounds, o.server_threads, o.load_threads
    ));
    md.push_str("| scenario | framework | req/s | vs best | p50 us | p99 us | max us | RSS MiB | errors |\n|---|---|---:|---:|---:|---:|---:|---:|---:|\n");
    for (scenario, by_fw) in &results {
        let best = by_fw.values().filter_map(|v| v["rps_median"].as_f64()).fold(0.0, f64::max);
        for (fw, v) in by_fw {
            let rps = v["rps_median"].as_f64().unwrap_or(0.0);
            md.push_str(&format!(
                "| {scenario} | {fw} | {:.0} | {:.0}% | {} | {} | {} | {} | {} |\n",
                rps,
                if best > 0.0 { rps / best * 100.0 } else { 0.0 },
                v["latency_us"]["p50"],
                v["latency_us"]["p99"],
                v["latency_us"]["max"],
                v["rss_mib"].as_f64().map_or("-".into(), |m| format!("{m:.1}")),
                v["errors"],
            ));
        }
    }
    md.push_str("\nClient and servers share this machine and its cores; absolute numbers depend on it, ratios between rows are what carries over. Every server runs the same scenario with default settings.\n");

    println!("{md}");
    if std::fs::create_dir_all(&o.out).is_ok() {
        let stem = format!("{}-{}", started, command_output("hostname", &[]).unwrap_or_else(|| "host".into()).replace(['/', ' '], "_"));
        let json = serde_json::json!({ "machine": machine, "settings": {
            "duration_s": o.duration, "warmup_s": o.warmup, "rounds": o.rounds, "connections": o.connections,
            "server_threads": o.server_threads, "load_threads": o.load_threads }, "results": results });
        let _ = std::fs::write(o.out.join(format!("{stem}.json")), serde_json::to_string_pretty(&json).unwrap_or_default());
        let _ = std::fs::write(o.out.join(format!("{stem}.md")), &md);
        eprintln!("results written to {}/{stem}.{{json,md}}", o.out.display());
    }
}
