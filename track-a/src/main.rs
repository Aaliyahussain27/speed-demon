//! Track A (Ingest): summarize request log lines per (hour, endpoint).
//!
//! Stage 2 version. Changes from stage 1:
//!   - the input is read through a 1 MB buffer, not loaded into one String
//!   - timestamp, status and latency are read from bytes, not parsed as text
//!   - the hour is a number (slot 0..95), not a String
//!   - each endpoint gets a number the first time it is seen, so boxes live in a flat Vec
//!   - latencies are stored as u32 (the spec says they fit)
//!
//! Usage:
//!   solution                      read /data/requests.log, write /out/summary.txt
//!   solution <input> <output>     read a file, write a file
//!   solution - <output>           read the log from stdin (for streaming)

use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;

/// Hour slots: 4 calendar days x 24 hours. The data uses 72 of the 96.
const HOURS: usize = 96;
/// Date text for each day ordinal (0 = 2026-10-30).
const DAYS: [&str; 4] = ["2026-10-30", "2026-10-31", "2026-11-01", "2026-11-02"];

/// Everything we keep for one (endpoint, hour) box.
#[derive(Default)]
struct Group {
    /// Responses with status 500 to 599.
    errors: u32,
    /// Every latency in this box, in microseconds. Duplicates are kept.
    latencies: Vec<u32>,
}

struct State {
    /// Endpoint bytes -> endpoint id.
    ids: HashMap<Box<[u8]>, u32>,
    /// Endpoint id -> endpoint bytes.
    names: Vec<Box<[u8]>>,
    /// Flat table. Index = endpoint_id * HOURS + hour_slot.
    groups: Vec<Group>,
}

impl State {
    fn new() -> Self {
        State {
            ids: HashMap::new(),
            names: Vec::new(),
            groups: Vec::new(),
        }
    }

    /// Return the id for this endpoint, giving it a new id the first time it appears.
    fn endpoint_id(&mut self, endpoint: &[u8]) -> usize {
        if let Some(&id) = self.ids.get(endpoint) {
            return id as usize;
        }
        let id = self.names.len() as u32;
        self.names.push(endpoint.into());
        self.ids.insert(endpoint.into(), id);
        // Make room for one box per hour slot for this new endpoint.
        self.groups.resize_with(self.groups.len() + HOURS, Group::default);
        id as usize
    }
}

/// Turn ASCII digits into a number, reading byte by byte. No text parsing.
fn digits(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .fold(0u64, |acc, &c| acc * 10 + (c - b'0') as u64) as u32
}

/// Turn a timestamp like "2026-10-30T18:07:18.569411Z" into an hour slot.
/// The slot is day ordinal * 24 + hour. Minutes and seconds are ignored, which rounds down.
fn hour_slot(ts: &[u8]) -> usize {
    // Positions: 0-3 year, 5-6 month, 8-9 day, 11-12 hour.
    let month = digits(&ts[5..7]);
    let day = digits(&ts[8..10]);
    let hour = digits(&ts[11..13]);
    let ordinal = if month == 10 { day - 30 } else { day + 1 };
    assert!(ordinal < 4, "timestamp outside the 72-hour window");
    ordinal as usize * 24 + hour as usize
}

/// Nearest-rank percentile of a sorted list (rank = ceil(p * n / 100)).
fn percentile(sorted: &[u32], p: u64) -> u32 {
    let n = sorted.len() as u64;
    let rank = (p * n + 99) / 100;
    sorted[(rank - 1) as usize]
}

/// Handle one line (without its newline). Line format: timestamp method endpoint status latency_us.
fn process_line(line: &[u8], st: &mut State) {
    // splitn works on byte slices, so it does not allocate new strings.
    let mut fields = line.splitn(5, |&b| b == b' ');
    let ts = fields.next().unwrap();
    let _method = fields.next().unwrap(); // not needed for Track A
    let endpoint = fields.next().unwrap();
    let status = digits(fields.next().unwrap());
    let latency = digits(fields.next().unwrap());

    let slot = hour_slot(ts);
    let eid = st.endpoint_id(endpoint);

    let group = &mut st.groups[eid * HOURS + slot];
    group.latencies.push(latency);
    if (500..=599).contains(&status) {
        group.errors += 1;
    }
}

/// Build the output text: one line per non-empty box, hour first, then endpoint bytes.
fn render(st: &mut State) -> Vec<u8> {
    // Endpoint ids in the byte order of their names.
    let mut order: Vec<usize> = (0..st.names.len()).collect();
    order.sort_by(|&a, &b| st.names[a].cmp(&st.names[b]));

    let mut out = Vec::new();
    for slot in 0..HOURS {
        for &eid in &order {
            let group = &mut st.groups[eid * HOURS + slot];
            if group.latencies.is_empty() {
                continue;
            }
            group.latencies.sort_unstable();

            let count = group.latencies.len() as u64;
            let p50 = percentile(&group.latencies, 50);
            let p95 = percentile(&group.latencies, 95);
            let p99 = percentile(&group.latencies, 99);
            // Error rate in parts per million, rounded down.
            let error_ppm = group.errors as u64 * 1_000_000 / count;

            writeln!(
                out,
                "{}T{:02}:00:00Z {} {} {} {} {} {} {}",
                DAYS[slot / 24],
                slot % 24,
                String::from_utf8_lossy(&st.names[eid]),
                count,
                p50,
                p95,
                p99,
                group.errors,
                error_ppm
            )
            .unwrap();
        }
    }
    out
}

/// Open the input as a buffered reader. "-" means standard input.
/// The 1 MB buffer means the file is read in chunks, not all at once.
fn open_input(path: &str) -> io::Result<Box<dyn BufRead>> {
    if path == "-" {
        Ok(Box::new(BufReader::with_capacity(1 << 20, io::stdin().lock())))
    } else {
        Ok(Box::new(BufReader::with_capacity(1 << 20, File::open(path)?)))
    }
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let input = args.get(1).map(String::as_str).unwrap_or("/data/requests.log");
    let output = args.get(2).map(String::as_str).unwrap_or("/out/summary.txt");

    let mut reader = open_input(input)?;
    let mut st = State::new();
    let mut line = Vec::with_capacity(256);

    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break; // end of input
        }
        // Remove the line ending. Some files use \r\n, so strip \r too.
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if !line.is_empty() {
            process_line(&line, &mut st);
        }
    }

    let out = render(&mut st);
    if let Some(dir) = Path::new(output).parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(output, out)
}