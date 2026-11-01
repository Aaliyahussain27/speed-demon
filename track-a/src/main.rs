//! Track A (Ingest): parallel version of the stage 4 flat-table code.
//!
//! How it works:
//! 1. Read the input in 16 MB blocks. Cut each block after its last line break,
//!    so every block holds whole lines only.
//! 2. Split each block into one part per thread, cut at line breaks.
//! 3. Each thread fills its own table (endpoint ids and boxes).
//! 4. Merge the per-thread tables into one table, then compute the percentiles.
//!
//! Output line: `hour endpoint count p50 p95 p99 errors error_ppm`

use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;
use std::thread;

/// Hour slots: 4 calendar days x 24 hours.
const HOURS: usize = 96;
/// Date text for each day ordinal (0 = 2026-10-30).
const DAYS: [&str; 4] = ["2026-10-30", "2026-10-31", "2026-11-01", "2026-11-02"];
/// Bytes read per block (4 MB: small enough to keep memory low).
const BLOCK: usize = 4 << 20;

#[derive(Default)]
struct Group {
    errors: u32,
    latencies: Vec<u32>,
}

#[derive(Default)]
struct State {
    /// Endpoint bytes -> endpoint id.
    ids: HashMap<Box<[u8]>, u32>,
    /// Endpoint id -> endpoint bytes.
    names: Vec<Box<[u8]>>,
    /// Flat table. Index = endpoint_id * HOURS + hour_slot.
    groups: Vec<Group>,
}

impl State {
    fn endpoint_id(&mut self, endpoint: &[u8]) -> usize {
        if let Some(&id) = self.ids.get(endpoint) {
            return id as usize;
        }
        let id = self.names.len() as u32;
        self.names.push(endpoint.into());
        self.ids.insert(endpoint.into(), id);
        self.groups.resize_with(self.groups.len() + HOURS, Group::default);
        id as usize
    }
}

fn digits(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .fold(0u64, |acc, &c| acc * 10 + (c - b'0') as u64) as u32
}

fn hour_slot(ts: &[u8]) -> usize {
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

/// One line without its line break. Format: timestamp method endpoint status latency_us.
fn process_line(line: &[u8], st: &mut State) {
    let mut fields = line.splitn(5, |&b| b == b' ');
    let ts = fields.next().unwrap();
    let _method = fields.next().unwrap();
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

/// Add one thread's table into the global table.
fn absorb(global: &mut State, mut local: State) {
    for (lid, name) in local.names.iter().enumerate() {
        let gid = global.endpoint_id(name);
        for slot in 0..HOURS {
            let src = &mut local.groups[lid * HOURS + slot];
            if src.latencies.is_empty() {
                continue;
            }
            let dst = &mut global.groups[gid * HOURS + slot];
            dst.errors += src.errors;
            dst.latencies.append(&mut src.latencies);
        }
    }
}

/// Process a block that ends with a line break (or is the last, partial block).
fn process_block(block: &[u8], threads: usize, global: &mut State) {
    // Cut points: each part starts right after a line break.
    let mut cuts = vec![0usize];
    for i in 1..threads {
        let target = block.len() * i / threads;
        let cut = match block[target..].iter().position(|&b| b == b'\n') {
            Some(p) => target + p + 1,
            None => block.len(),
        };
        if cut > *cuts.last().unwrap() {
            cuts.push(cut);
        }
    }
    cuts.push(block.len());

    let locals: Vec<State> = thread::scope(|s| {
        let handles: Vec<_> = cuts
            .windows(2)
            .map(|w| {
                let part = &block[w[0]..w[1]];
                s.spawn(move || {
                    let mut st = State::default();
                    for line in part.split(|&b| b == b'\n') {
                        let line = line.strip_suffix(&[b'\r']).unwrap_or(line);
                        if !line.is_empty() {
                            process_line(line, &mut st);
                        }
                    }
                    st
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    for local in locals {
        absorb(global, local);
    }
}

/// Fill `buf` as far as possible. Returns fewer bytes only at the end of input.
fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        let n = r.read(&mut buf[total..])?;
        if n == 0 {
            break;
        }
        total += n;
    }
    Ok(total)
}

fn process_input<R: Read>(mut reader: R, threads: usize) -> io::Result<State> {
    let mut global = State::default();
    let mut chunk = vec![0u8; BLOCK];
    let mut carry: Vec<u8> = Vec::new();
    loop {
        let n = read_full(&mut reader, &mut chunk)?;
        if n == 0 {
            if !carry.is_empty() {
                process_block(&carry, threads, &mut global);
            }
            break;
        }
        let mut data = std::mem::take(&mut carry);
        data.extend_from_slice(&chunk[..n]);
        match data.iter().rposition(|&b| b == b'\n') {
            Some(p) => {
                carry = data[p + 1..].to_vec();
                data.truncate(p + 1);
                process_block(&data, threads, &mut global);
            }
            None => carry = data, // no full line yet: keep reading
        }
    }
    Ok(global)
}

fn render(st: &mut State) -> Vec<u8> {
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

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let input = args.get(1).map(String::as_str).unwrap_or("/data/requests.log");
    let output = args.get(2).map(String::as_str).unwrap_or("/out/summary.txt");

    let threads = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 12);

    let mut st = if input == "-" {
        process_input(io::stdin().lock(), threads)?
    } else {
        process_input(File::open(input)?, threads)?
    };

    let out = render(&mut st);
    if let Some(dir) = Path::new(output).parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(output, out)
}