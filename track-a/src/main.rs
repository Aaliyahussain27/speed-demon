//! Track A (Ingest), flat-array version.
//!
//! Pass 1: read every line into a small record (box key, latency) and count errors.
//! Pass 2: counting sort, so each box's latencies sit next to each other in one array.
//! Then sort each box's slice and read the percentiles.

use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;

/// Hour slots: 4 calendar days x 24 hours. The data uses 72 of the 96.
const HOURS: usize = 96;
/// Date text for each day ordinal (0 = 2026-10-30).
const DAYS: [&str; 4] = ["2026-10-30", "2026-10-31", "2026-11-01", "2026-11-02"];

/// One request, reduced to what we need: its box key and its latency.
/// key = endpoint_id * HOURS + hour_slot.
#[derive(Clone, Copy)]
struct Rec {
    key: u32,
    latency: u32,
}

/// Everything we collect while reading the input.
struct Parsed {
    /// Endpoint bytes -> endpoint id.
    ids: HashMap<Box<[u8]>, u32>,
    /// Endpoint id -> endpoint bytes.
    names: Vec<Box<[u8]>>,
    /// One record per request, in input order.
    recs: Vec<Rec>,
    /// Error count per box. Index = key.
    errors: Vec<u32>,
}

impl Parsed {
    /// Return the id for this endpoint, giving it a new id the first time it appears.
    fn endpoint_id(&mut self, endpoint: &[u8]) -> u32 {
        if let Some(&id) = self.ids.get(endpoint) {
            return id;
        }
        let id = self.names.len() as u32;
        self.names.push(endpoint.into());
        self.ids.insert(endpoint.into(), id);
        // Room for one box per hour slot for this new endpoint.
        self.errors.resize(self.errors.len() + HOURS, 0);
        id
    }
}

/// Turn ASCII digits into a number, reading byte by byte.
fn digits(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .fold(0u64, |acc, &c| acc * 10 + (c - b'0') as u64) as u32
}

/// Turn a timestamp like "2026-10-30T18:07:18.569411Z" into an hour slot.
/// Slot = day ordinal * 24 + hour. Minutes and seconds are ignored, which rounds down.
fn hour_slot(ts: &[u8]) -> usize {
    // Positions: 0-3 year, 5-6 month, 8-9 day, 11-12 hour.
    let month = digits(&ts[5..7]);
    let day = digits(&ts[8..10]);
    let hour = digits(&ts[11..13]);
    let ordinal = if month == 10 { day - 30 } else { day + 1 };
    assert!(ordinal < 4, "timestamp outside the 72-hour window");
    ordinal as usize * 24 + hour as usize
}

/// Nearest-rank percentile of a sorted slice (rank = ceil(p * n / 100)).
fn percentile(sorted: &[u32], p: u64) -> u32 {
    let n = sorted.len() as u64;
    let rank = (p * n + 99) / 100;
    sorted[(rank - 1) as usize]
}

/// Handle one line (without its newline). Format: timestamp method endpoint status latency_us.
fn process_line(line: &[u8], p: &mut Parsed) {
    let mut fields = line.splitn(5, |&b| b == b' ');
    let ts = fields.next().unwrap();
    let _method = fields.next().unwrap(); // not needed for Track A
    let endpoint = fields.next().unwrap();
    let status = digits(fields.next().unwrap());
    let latency = digits(fields.next().unwrap());

    let key = p.endpoint_id(endpoint) * HOURS as u32 + hour_slot(ts) as u32;
    if (500..=599).contains(&status) {
        p.errors[key as usize] += 1;
    }
    p.recs.push(Rec { key, latency });
}

/// Counting sort: put every latency into one array, grouped by box.
/// Returns (offsets, values). Box k's latencies are values[offsets[k]..offsets[k + 1]].
fn group_by_box(p: &Parsed) -> (Vec<u32>, Vec<u32>) {
    let n_keys = p.errors.len();

    // Count how many requests each box has. Shift by one so the prefix sum gives start positions.
    let mut offsets = vec![0u32; n_keys + 1];
    for r in &p.recs {
        offsets[r.key as usize + 1] += 1;
    }
    for k in 0..n_keys {
        offsets[k + 1] += offsets[k];
    }

    // Place each latency at the next free slot of its box.
    let mut next = offsets.clone();
    let mut values = vec![0u32; p.recs.len()];
    for r in &p.recs {
        let k = r.key as usize;
        values[next[k] as usize] = r.latency;
        next[k] += 1;
    }
    (offsets, values)
}

/// Build the output text: one line per non-empty box, hour first, then endpoint bytes.
fn render(p: &Parsed, offsets: &[u32], values: &mut [u32]) -> Vec<u8> {
    // Endpoint ids in the byte order of their names.
    let mut order: Vec<usize> = (0..p.names.len()).collect();
    order.sort_by(|&a, &b| p.names[a].cmp(&p.names[b]));

    let mut out = Vec::new();
    for slot in 0..HOURS {
        for &eid in &order {
            let key = eid * HOURS + slot;
            let start = offsets[key] as usize;
            let end = offsets[key + 1] as usize;
            if start == end {
                continue; // empty box, no line
            }

            values[start..end].sort_unstable();
            let lat: &[u32] = &values[start..end];

            let count = lat.len() as u64;
            let p50 = percentile(lat, 50);
            let p95 = percentile(lat, 95);
            let p99 = percentile(lat, 99);
            let errors = p.errors[key] as u64;
            // Error rate in parts per million, rounded down.
            let error_ppm = errors * 1_000_000 / count;

            writeln!(
                out,
                "{}T{:02}:00:00Z {} {} {} {} {} {} {}",
                DAYS[slot / 24],
                slot % 24,
                String::from_utf8_lossy(&p.names[eid]),
                count,
                p50,
                p95,
                p99,
                errors,
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
    let mut p = Parsed {
        ids: HashMap::new(),
        names: Vec::new(),
        recs: Vec::with_capacity(3_200_000),
        errors: Vec::new(),
    };
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
            process_line(&line, &mut p);
        }
    }

    let (offsets, mut values) = group_by_box(&p);
    // The records are no longer needed, so give their memory back.
    p.recs = Vec::new();

    let out = render(&p, &offsets, &mut values);
    if let Some(dir) = Path::new(output).parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(output, out)
}