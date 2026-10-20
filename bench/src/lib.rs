//! Shared types of the benchmark tools

pub mod load;

use serde::{Deserialize, Serialize};

/// What the load generator measured in one run
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadResult {
    pub requests: u64,
    pub errors: u64,
    pub duration_s: f64,
    pub rps: f64,
    pub bytes: u64,
    pub latency_us: Latency,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Latency {
    pub mean: f64,
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
    pub p999: u64,
    pub max: u64,
}

pub const FRAMEWORKS: [&str; 3] = ["ullun", "axum", "actix"];
pub const SCENARIOS: [&str; 5] = ["hello", "json", "param", "post", "middleware"];

/// Request a scenario's load generator should send: (method, path, body)
pub fn request_for(scenario: &str) -> (&'static str, &'static str, &'static str) {
    match scenario {
        "hello" => ("GET", "/", ""),
        "json" => ("GET", "/json", ""),
        "param" => ("GET", "/users/42", ""),
        "post" => ("POST", "/echo", r#"{"name":"Alice","age":30,"tags":["a","b","c"]}"#),
        "middleware" => ("GET", "/", ""),
        _ => ("GET", "/", ""),
    }
}

pub fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    match v.len() {
        0 => 0.0,
        n if n % 2 == 1 => v[n / 2],
        n => (v[n / 2 - 1] + v[n / 2]) / 2.0,
    }
}
