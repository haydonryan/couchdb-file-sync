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

fn main() {
    let root = PathBuf::from("target/scanner-perf-data");
    let files: usize = std::env::var("SCANNER_PERF_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000);
    let bytes: usize = std::env::var("SCANNER_PERF_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4_096);

    make_dataset(&root, files, bytes);

    let scanner = Scanner::new(root.clone(), IgnoreMatcher::empty());

    let cold_start = Instant::now();
    let cold_states = scanner.full_scan().unwrap();
    let cold_elapsed = cold_start.elapsed();

    let warm_start = Instant::now();
    let warm_states = scanner.full_scan_with_previous(&cold_states).unwrap();
    let warm_elapsed = warm_start.elapsed();

    println!(
        "files={} bytes_each={} cold_ms={} warm_ms={} cold_per_file_us={} warm_per_file_us={} warm_states={}",
        files,
        bytes,
        cold_elapsed.as_millis(),
        warm_elapsed.as_millis(),
        cold_elapsed.as_micros() / cold_states.len() as u128,
        warm_elapsed.as_micros() / warm_states.len() as u128,
        warm_states.len()
    );
}
