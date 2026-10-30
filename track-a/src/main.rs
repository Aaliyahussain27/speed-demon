//! Track A (Ingest): summarize request log lines per (hour, endpoint).
//!
//! Output line: `hour endpoint count p50 p95 p99 errors error_ppm`
//!
//! This is the stage 1 version: simple and correct. Speed comes later,
//! one measured change at a time (see PERF.md).
//!
//! Usage:
//!   solution                      read /data/requests.log, write /out/summary.txt
//!   solution <input> <output>     read a file, write a file
//!   solution - <output>           read the log from stdin (for streaming)

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::Path;

/// Everything we remember about one (hour, endpoint) box.
struct Group {
    /// Number of responses with status 500 to 599.
    errors: u64,
    /// Every latency in this box, in microseconds. Duplicates are kept.
    latencies: Vec<u64>,
}

/// Nearest-rank percentile of an already sorted list.
/// The 1-indexed rank is ceil(p * n / 100), written with integer math.
fn percentile(sorted: &[u64], p: u64) -> u64 {
    let n = sorted.len() as u64;
    let rank = (p * n + 99) / 100;
    sorted[(rank - 1) as usize]
}

/// Read the whole input. "-" means standard input (a pipe).
fn read_input(path: &str) -> io::Result<String> {
    if path == "-" {
        let mut text = String::new();
        io::stdin().read_to_string(&mut text)?;
        Ok(text)
    } else {
        fs::read_to_string(path)
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let input = args.get(1).map(String::as_str).unwrap_or("/data/requests.log");
    let output = args.get(2).map(String::as_str).unwrap_or("/out/summary.txt");

    let text = read_input(input).expect("cannot read input");

    // BTreeMap keeps its keys sorted, so the output comes out in the right order.
    // Key = (hour text, endpoint text). Hour text is fixed width, so text order is time order.
    let mut groups: BTreeMap<(String, String), Group> = BTreeMap::new();

    for line in text.lines() {
        // Line format: timestamp method endpoint status latency_us (single spaces).
        let mut parts = line.split(' ');
        let ts = parts.next().unwrap();
        let _method = parts.next().unwrap(); // not needed for Track A
        let endpoint = parts.next().unwrap();
        let status: u64 = parts.next().unwrap().parse().unwrap();
        let latency: u64 = parts.next().unwrap().parse().unwrap();

        // "2026-10-30T18:07:18.569411Z" -> first 13 chars are "2026-10-30T18".
        // Rounding down to the hour means the minutes, seconds and microseconds are dropped.
        let hour = format!("{}:00:00Z", &ts[0..13]);

        let group = groups
            .entry((hour, endpoint.to_string()))
            .or_insert_with(|| Group {
                errors: 0,
                latencies: Vec::new(),
            });

        group.latencies.push(latency);
        if (500..=599).contains(&status) {
            group.errors += 1;
        }
    }

    // Build the output, one line per box, in key order.
    let mut out = String::new();
    for ((hour, endpoint), group) in groups.iter_mut() {
        group.latencies.sort_unstable();

        let count = group.latencies.len() as u64;
        let p50 = percentile(&group.latencies, 50);
        let p95 = percentile(&group.latencies, 95);
        let p99 = percentile(&group.latencies, 99);
        // Error rate in parts per million, rounded down, integer math only.
        let error_ppm = group.errors * 1_000_000 / count;

        out.push_str(&format!(
            "{} {} {} {} {} {} {} {}\n",
            hour, endpoint, count, p50, p95, p99, group.errors, error_ppm
        ));
    }

    if let Some(dir) = Path::new(output).parent() {
        fs::create_dir_all(dir).expect("cannot create output folder");
    }
    fs::write(output, out).expect("cannot write output");
}