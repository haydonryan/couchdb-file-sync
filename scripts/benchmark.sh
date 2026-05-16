#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

ITERATIONS="${BENCH_ITERATIONS:-50}"
WARMUP="${BENCH_WARMUP:-5}"
REPORT_DIR="${BENCH_REPORT_DIR:-dist/benchmark}"
BUILD_COMMAND="${BENCH_BUILD_COMMAND:-cargo build --release --workspace}"
HTTP_COMMAND="${BENCH_HTTP_COMMAND:-}"
HTTP_PATHS="${BENCH_HTTP_PATHS:-/ /health}"
APP_VERSION="${APP_VERSION:-benchmark}"

command_required() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "Missing required command: $1" >&2
    exit 1
  fi
}

free_port() {
  python3 - <<'PY_FREE_PORT'
import socket
with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY_FREE_PORT
}

elapsed_ms_for() {
  local output_file="$1"
  shift
  local start end
  start="$(date +%s%N)"
  bash -euo pipefail -c "$*" >"$output_file" 2>&1
  end="$(date +%s%N)"
  python3 - "$start" "$end" <<'PY_ELAPSED'
import sys
start = int(sys.argv[1])
end = int(sys.argv[2])
print(f"{(end - start) / 1_000_000:.0f}")
PY_ELAPSED
}

release_binary_summary() {
  python3 - <<'PY_BINARIES'
import json
import subprocess
from pathlib import Path

metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--format-version', '1', '--no-deps'], text=True))
target_dir = Path(metadata['target_directory']) / 'release'
binaries = []
seen = set()
for package in metadata['packages']:
    for target in package.get('targets', []):
        if 'bin' not in target.get('kind', []):
            continue
        name = target['name']
        path = target_dir / name
        if path in seen:
            continue
        seen.add(path)
        size = path.stat().st_size if path.exists() else 0
        gzip_size = 0
        if path.exists():
            gzip_size = int(subprocess.check_output(['bash', '-c', 'gzip -c -9 "$1" | wc -c', 'bash', str(path)], text=True).strip())
        binaries.append({'name': name, 'path': str(path), 'exists': path.exists(), 'bytes': size, 'gzip_bytes': gzip_size})
print(json.dumps(binaries, indent=2, sort_keys=True))
PY_BINARIES
}

wait_for_health() {
  local url="$1"
  local health_path="${BENCH_HEALTH_PATH:-/health}"
  for _ in $(seq 1 100); do
    if curl --silent --fail --max-time 1 "$url$health_path" >/dev/null; then
      return 0
    fi
    sleep 0.1
  done
  echo "Timed out waiting for $url$health_path" >&2
  return 1
}

http_bench() {
  python3 - "$BASE_URL" "$ITERATIONS" "$WARMUP" "$HTTP_PATHS" <<'PY_HTTP'
from concurrent.futures import ThreadPoolExecutor
from statistics import mean, median
import json
import sys
import time
import urllib.request

base_url = sys.argv[1].rstrip('/')
iterations = int(sys.argv[2])
warmup = int(sys.argv[3])
paths = [path for path in sys.argv[4].split() if path]

def percentile(values, pct):
    ordered = sorted(values)
    index = (len(ordered) - 1) * pct / 100
    lower = int(index)
    upper = min(lower + 1, len(ordered) - 1)
    if lower == upper:
        return ordered[lower]
    return ordered[lower] + (ordered[upper] - ordered[lower]) * (index - lower)

def fetch(path):
    started = time.perf_counter_ns()
    with urllib.request.urlopen(f'{base_url}{path}', timeout=10) as response:
        body = response.read()
        status = response.status
    return {'status': status, 'latency_ms': (time.perf_counter_ns() - started) / 1_000_000, 'bytes': len(body)}

results = {}
for path in paths:
    for _ in range(warmup):
        fetch(path)
    samples = [fetch(path) for _ in range(iterations)]
    latencies = [sample['latency_ms'] for sample in samples]
    results[path] = {'iterations': iterations, 'status': samples[-1]['status'], 'response_bytes': samples[-1]['bytes'], 'latency_ms': {'min': min(latencies), 'avg': mean(latencies), 'p50': median(latencies), 'p95': percentile(latencies, 95), 'max': max(latencies)}}
if paths:
    path = paths[-1]
    started = time.perf_counter_ns()
    with ThreadPoolExecutor(max_workers=10) as pool:
        concurrent = list(pool.map(lambda _: fetch(path), range(iterations)))
    elapsed_s = (time.perf_counter_ns() - started) / 1_000_000_000
    latencies = [sample['latency_ms'] for sample in concurrent]
    results[f'{path} concurrent x10'] = {'iterations': iterations, 'requests_per_second': iterations / elapsed_s if elapsed_s else 0, 'latency_ms': {'min': min(latencies), 'avg': mean(latencies), 'p50': median(latencies), 'p95': percentile(latencies, 95), 'max': max(latencies)}}
print(json.dumps(results, indent=2, sort_keys=True))
PY_HTTP
}

print_human_summary() {
  python3 - "$REPORT_DIR/report.json" <<'PY_SUMMARY'
import json
import sys
report = json.load(open(sys.argv[1]))
def ms(value): return f"{value:.1f}ms"
def kb(value): return f"{value / 1024:.1f} KiB"
print('\nBenchmark summary')
print('=================')
print(f"build: {ms(report['build']['elapsed_ms'])}; command: {report['build']['command']}")
for binary in report['binaries']:
    state = 'ok' if binary['exists'] else 'missing'
    print(f"{binary['name']}: {state}, {kb(binary['bytes'])}, gzip {kb(binary['gzip_bytes'])}")
if report.get('latency'):
    for path, stats in report['latency'].items():
        latency = stats['latency_ms']
        suffix = f", {stats['requests_per_second']:.1f} req/s" if 'requests_per_second' in stats else ''
        print(f"{path}: p50 {ms(latency['p50'])}, p95 {ms(latency['p95'])}, avg {ms(latency['avg'])}{suffix}")
print(f"\nFull JSON: {sys.argv[1]}")
PY_SUMMARY
}

command_required cargo
command_required gzip
command_required python3
if [ -n "$HTTP_COMMAND" ]; then
  command_required curl
fi

mkdir -p "$REPORT_DIR"
build_log="$REPORT_DIR/build.log"
build_ms="$(elapsed_ms_for "$build_log" "$BUILD_COMMAND")"
binaries_json="$REPORT_DIR/binaries.json"
release_binary_summary >"$binaries_json"
latency_json=""
server_pid=""
cleanup() {
  if [ -n "$server_pid" ] && kill -0 "$server_pid" >/dev/null 2>&1; then
    kill "$server_pid" >/dev/null 2>&1 || true
    wait "$server_pid" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

if [ -n "$HTTP_COMMAND" ]; then
  port="${PORT:-$(free_port)}"
  BASE_URL="${BENCH_BASE_URL:-http://127.0.0.1:$port}"
  HOST="${HOST:-127.0.0.1}" PORT="$port" APP_VERSION="$APP_VERSION" bash -euo pipefail -c "$HTTP_COMMAND" >"$REPORT_DIR/server.log" 2>&1 &
  server_pid="$!"
  wait_for_health "$BASE_URL"
  latency_json="$REPORT_DIR/http-latency.json"
  http_bench >"$latency_json"
fi

python3 - "$REPORT_DIR/report.json" "$BUILD_COMMAND" "$build_ms" "$binaries_json" "$latency_json" "$ITERATIONS" "$WARMUP" <<'PY_REPORT'
import json
import sys
out_path, build_command, build_ms, binaries_path, latency_path, iterations, warmup = sys.argv[1:]
report = {'build': {'command': build_command, 'elapsed_ms': float(build_ms)}, 'binaries': json.load(open(binaries_path)), 'settings': {'bench_iterations': int(iterations), 'bench_warmup': int(warmup)}}
if latency_path:
    report['latency'] = json.load(open(latency_path))
with open(out_path, 'w') as handle:
    json.dump(report, handle, indent=2, sort_keys=True)
    handle.write('\n')
PY_REPORT

print_human_summary
