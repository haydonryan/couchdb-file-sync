use couchdb_file_sync::local::Scanner;
use couchdb_file_sync::models::IgnoreMatcher;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

fn make_dataset(root: &PathBuf, files: usize, bytes: usize) {
    if root.exists() {
        fs::remove_dir_all(root).unwrap();
    }
    fs::create_dir_all(root).unwrap();
    let chunk = vec![b'x'; bytes];

    for i in 0..files {
        let dir = root.join(format!("dir-{:03}", i % 50));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("file-{i:05}.bin")), &chunk).unwrap();
    }
}

fn parse_env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let root = PathBuf::from("target/scanner-perf-data");
    let files = parse_env_usize("SCANNER_PERF_FILES", 2_000);
    let bytes = parse_env_usize("SCANNER_PERF_BYTES", 4_096);
    let iterations = parse_env_usize("SCANNER_PERF_ITERATIONS", 1_000);

    make_dataset(&root, files, bytes);

    let scanner = Scanner::new(root.clone(), IgnoreMatcher::empty());

    let cold_start = Instant::now();
    let cold_states = scanner.full_scan().unwrap();
    let cold_elapsed = cold_start.elapsed();

    let warm_start = Instant::now();
    let warm_states = scanner.full_scan_with_previous(&cold_states).unwrap();
    let warm_elapsed = warm_start.elapsed();

    let mut repeated_warm_runs_us = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let repeated_start = Instant::now();
        let repeated_states = scanner.full_scan_with_previous(&warm_states).unwrap();
        repeated_warm_runs_us.push(repeated_start.elapsed().as_micros());
        assert_eq!(
            repeated_states.len(),
            warm_states.len(),
            "warm scan state count changed across repeated runs"
        );
    }

    repeated_warm_runs_us.sort_unstable();
    let repeated_total_us: u128 = repeated_warm_runs_us.iter().copied().sum();
    let repeated_avg_us = repeated_total_us / iterations as u128;
    let repeated_min_us = repeated_warm_runs_us[0];
    let repeated_median_us = repeated_warm_runs_us[iterations / 2];
    let repeated_max_us = repeated_warm_runs_us[iterations - 1];

    println!(
        "files={} bytes_each={} iterations={} cold_ms={} warm_ms={} cold_per_file_us={} warm_per_file_us={} repeated_warm_min_us={} repeated_warm_median_us={} repeated_warm_avg_us={} repeated_warm_max_us={} repeated_warm_avg_per_file_us={} warm_states={}",
        files,
        bytes,
        iterations,
        cold_elapsed.as_millis(),
        warm_elapsed.as_millis(),
        cold_elapsed.as_micros() / cold_states.len() as u128,
        warm_elapsed.as_micros() / warm_states.len() as u128,
        repeated_min_us,
        repeated_median_us,
        repeated_avg_us,
        repeated_max_us,
        repeated_avg_us / warm_states.len() as u128,
        warm_states.len()
    );
}
