#!/usr/bin/env bash
# Portable framework comparison: ullun-api vs axum vs actix-web on this machine.
#
#   scripts/bench.sh [bench-run options]     e.g. --duration 10 --rounds 5
#   scripts/bench.sh --quick                 smoke test (about 30 s)
#   scripts/bench.sh --micro                 also run the criterion micro-benchmarks
#
# Needs only a Rust toolchain. Results (JSON + Markdown) go to bench/results/.
# For stable numbers: close other apps, plug in power, and do not run it on a
# shared CI runner. Send the .md file from bench/results/ when comparing machines.
set -euo pipefail
cd "$(dirname "$0")/.."

micro=0
args=()
for a in "$@"; do
  if [ "$a" = "--micro" ]; then micro=1; else args+=("$a"); fi
done

cargo build --release --manifest-path bench/Cargo.toml
(cd bench && ./target/release/bench-run ${args[@]+"${args[@]}"})

if [ "$micro" = 1 ]; then
  cargo bench --bench router_benchmark
  cargo bench --bench http_benchmark
fi
