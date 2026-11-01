//! Track C (Join): parse a hostile CSV, join it to a region lookup, write per-country aggregates.
//!
//! How it works:
//! 1. Both files are read into memory once. records.csv is about 200 MB.
//! 2. The region lookup is a plain array indexed by region_id (ids are at most 9,999,999).
//! 3. Parallel start: a byte is inside quotes exactly when an odd number of '"' bytes
//!    come before it (a "" pair toggles twice, so it cancels out). Each thread counts the
//!    quotes in its own chunk, a prefix XOR gives every chunk its starting state, and each
//!    chunk then finds the first record that starts inside it.
//! 4. Each thread parses its records byte by byte, adds them to per-country totals, and gives
//!    each distinct name a local id.
//! 5. Local ids are merged into one global id space. names_distinct is the number of bits
//!    set in each country's bitset.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::thread;

const MAX_REGION: usize = 10_000_000;
/// Marks a region id that is not in the lookup file.
const NO_COUNTRY: u16 = u16::MAX;
const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

#[derive(Clone)]
struct Agg {
    count: u64,
    id_sum: i64,
    amt_sum: i64,
    amt_min: i64,
    amt_max: i64,
    name_bytes: u64,
    note_bytes: u64,
    /// Bit i is set when this thread saw local name id i in this group.
    name_bits: Vec<u64>,
}

impl Agg {
    fn new() -> Self {
        Agg {
            count: 0,
            id_sum: 0,
            amt_sum: 0,
            amt_min: i64::MAX,
            amt_max: i64::MIN,
            name_bytes: 0,
            note_bytes: 0,
            name_bits: Vec::new(),
        }
    }
}

/// Everything one thread produces.
struct Worker {
    /// Slot index of _UNMATCHED (the last slot).
    unmatched: usize,
    aggs: Vec<Agg>,
    name_ids: HashMap<Box<[u8]>, u32>,
    /// Holds one unescaped name at a time.
    scratch: Vec<u8>,
}

impl Worker {
    fn new(nslots: usize) -> Self {
        Worker {
            unmatched: nslots - 1,
            aggs: vec![Agg::new(); nslots],
            name_ids: HashMap::new(),
            scratch: Vec::new(),
        }
    }
}

/// Digits to number, no text parsing.
fn parse_int(b: &[u8]) -> u64 {
    b.iter().fold(0u64, |acc, &c| acc * 10 + (c - b'0') as u64)
}

/// Parse "-?\d+\.\d{2}" into integer cents. The spec guarantees this format.
fn parse_money(b: &[u8]) -> i64 {
    let (neg, body) = match b.first() {
        Some(b'-') => (true, &b[1..]),
        _ => (false, b),
    };
    let dot = body.len() - 3;
    let whole = body[..dot]
        .iter()
        .fold(0i64, |acc, &c| acc * 10 + (c - b'0') as i64);
    let frac = (body[dot + 1] - b'0') as i64 * 10 + (body[dot + 2] - b'0') as i64;
    let cents = whole * 100 + frac;
    if neg {
        -cents
    } else {
        cents
    }
}

fn set_bit(bits: &mut Vec<u64>, id: u32) {
    let word = id as usize / 64;
    if bits.len() <= word {
        bits.resize(word + 1, 0);
    }
    bits[word] |= 1u64 << (id % 64);
}

/// Give a name its local id, the first time it is seen.
fn intern(ids: &mut HashMap<Box<[u8]>, u32>, name: &[u8]) -> u32 {
    if let Some(&id) = ids.get(name) {
        return id;
    }
    let id = ids.len() as u32;
    ids.insert(name.into(), id);
    id
}

/// Remove quoting from a raw value: every "" becomes one ". Raw quotes always come in pairs.
fn unescape_into(raw: &[u8], out: &mut Vec<u8>) {
    out.clear();
    let mut i = 0;
    while i < raw.len() {
        out.push(raw[i]);
        i += if raw[i] == b'"' { 2 } else { 1 };
    }
}

/// Read one text field starting at p.
/// Returns (value_start, value_end, number_of_"" pairs, index of the delimiter after it).
fn text_field(d: &[u8], p: usize) -> (usize, usize, usize, usize) {
    if d[p] == b'"' {
        let start = p + 1;
        let mut i = start;
        let mut pairs = 0;
        loop {
            let q = i + d[i..].iter().position(|&b| b == b'"').unwrap();
            if d.get(q + 1) == Some(&b'"') {
                pairs += 1;
                i = q + 2;
            } else {
                return (start, q, pairs, q + 1);
            }
        }
    } else {
        let mut i = p;
        while i < d.len() && !matches!(d[i], b',' | b'\r' | b'\n') {
            i += 1;
        }
        (p, i, 0, i)
    }
}

/// Parse one record that starts at p, add it to the worker's totals,
/// and return the index where the next record starts.
fn parse_record(d: &[u8], mut p: usize, region_map: &[u16], w: &mut Worker) -> usize {
    if d[p..].starts_with(BOM) {
        p += 3;
    }

    let id_end = p + d[p..].iter().position(|&b| b == b',').unwrap();
    let id = parse_int(&d[p..id_end]) as i64;
    p = id_end + 1;

    let (ns, ne, npairs, nnext) = text_field(d, p);
    p = nnext + 1;
    let (cs, ce, cpairs, cnext) = text_field(d, p);
    p = cnext + 1;

    let r_end = p + d[p..].iter().position(|&b| b == b',').unwrap();
    let region = parse_int(&d[p..r_end]) as usize;
    p = r_end + 1;

    let mut a_end = p;
    while a_end < d.len() && !matches!(d[a_end], b'\r' | b'\n') {
        a_end += 1;
    }
    let amount = parse_money(&d[p..a_end]);
    p = a_end;
    if p < d.len() && d[p] == b'\r' {
        p += 1;
    }
    if p < d.len() && d[p] == b'\n' {
        p += 1;
    }

    // Name: unescape only when it has "" pairs, then give it a local id.
    let name: &[u8] = if npairs == 0 {
        &d[ns..ne]
    } else {
        unescape_into(&d[ns..ne], &mut w.scratch);
        &w.scratch
    };
    let lid = intern(&mut w.name_ids, name);
    let name_len = name.len() as u64;

    // Join: region -> country slot.
    let country = if region < region_map.len() {
        region_map[region]
    } else {
        NO_COUNTRY
    };
    let slot = if country == NO_COUNTRY {
        w.unmatched
    } else {
        country as usize
    };

    let agg = &mut w.aggs[slot];
    agg.count += 1;
    agg.id_sum += id;
    agg.amt_sum += amount;
    agg.amt_min = agg.amt_min.min(amount);
    agg.amt_max = agg.amt_max.max(amount);
    agg.name_bytes += name_len;
    // Note bytes: raw length minus one per "" pair (an embedded LF counts as 1, as the spec says).
    agg.note_bytes += ((ce - cs) - cpairs) as u64;
    set_bit(&mut agg.name_bits, lid);
    p
}

/// Process every record that starts in [start, end).
fn run_chunk(
    data: &[u8],
    start: usize,
    end: usize,
    in_quotes: bool,
    region_map: &[u16],
    nslots: usize,
) -> Worker {
    let mut w = Worker::new(nslots);
    let first = if start == 0 {
        Some(0)
    } else if data[start - 1] == b'\n' && !in_quotes {
        // The byte before start is a line end outside quotes, so start is a record start.
        Some(start)
    } else {
        // Otherwise the first record starts after the first line end outside quotes.
        let mut inside = in_quotes;
        let mut found = None;
        for (i, &b) in data[start..].iter().enumerate() {
            match b {
                b'"' => inside = !inside,
                b'\n' if !inside => {
                    found = Some(start + i + 1);
                    break;
                }
                _ => {}
            }
        }
        found
    };
    if let Some(mut p) = first {
        while p < end && p < data.len() {
            p = parse_record(data, p, region_map, &mut w);
        }
    }
    w
}

/// Integer cents to "[-]int.dd", as the spec prints money.
fn money(c: i64) -> String {
    let sign = if c < 0 { "-" } else { "" };
    let a = c.abs();
    format!("{}{}.{:02}", sign, a / 100, a % 100)
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let records_path = args.get(1).map(String::as_str).unwrap_or("/data/records.csv");
    let regions_path = args.get(2).map(String::as_str).unwrap_or("/data/regions.csv");
    let output_path = args.get(3).map(String::as_str).unwrap_or("/out/aggregates.txt");

    // Region lookup: region id -> country slot.
    let regions = fs::read(regions_path)?;
    let mut region_map = vec![NO_COUNTRY; MAX_REGION];
    let mut codes: Vec<[u8; 3]> = Vec::new();
    let mut code_slot: HashMap<[u8; 3], usize> = HashMap::new();
    for line in regions.split(|&b| b == b'\n') {
        let line = line.strip_suffix(&[b'\r']).unwrap_or(line);
        if line.is_empty() || line.starts_with(b"region_id") {
            continue;
        }
        let comma = line.iter().position(|&b| b == b',').unwrap();
        let id = parse_int(&line[..comma]) as usize;
        let code: [u8; 3] = line[comma + 1..comma + 4].try_into().unwrap();
        let slot = *code_slot.entry(code).or_insert_with(|| {
            codes.push(code);
            codes.len() - 1
        });
        if id < MAX_REGION {
            region_map[id] = slot as u16;
        }
    }

    // One slot per country, plus the last slot for _UNMATCHED.
    let nslots = codes.len() + 1;
    let unmatched = nslots - 1;

    // Records: skip the optional BOM and the header line.
    let file = fs::read(records_path)?;
    let data_start = if file.starts_with(BOM) { 3 } else { 0 };
    let header_end = file[data_start..].iter().position(|&b| b == b'\n').unwrap();
    let data = &file[data_start + header_end + 1..];

    let threads = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(12)
        .clamp(1, 12);
    let bounds: Vec<usize> = (0..=threads).map(|i| i * data.len() / threads).collect();

    // Pass 1 (parallel): quote counts per chunk, then the quote state at each chunk start.
    let quote_counts: Vec<usize> = thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|i| {
                let chunk = &data[bounds[i]..bounds[i + 1]];
                s.spawn(move || chunk.iter().filter(|&&b| b == b'"').count())
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut parity = Vec::with_capacity(threads);
    let mut inside = false;
    for c in &quote_counts {
        parity.push(inside);
        inside ^= c % 2 == 1;
    }

    // Pass 2 (parallel): parse and aggregate each chunk.
    let workers: Vec<Worker> = thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|i| {
                let (start, end, in_quotes) = (bounds[i], bounds[i + 1], parity[i]);
                let region_map: &[u16] = &region_map;
                s.spawn(move || run_chunk(data, start, end, in_quotes, region_map, nslots))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Merge the threads' results.
    let mut global_ids: HashMap<Box<[u8]>, u32> = HashMap::new();
    let mut next_global_id = 0u32;
    let mut totals: Vec<Agg> = vec![Agg::new(); nslots];
    let mut distinct_bits: Vec<Vec<u64>> = vec![Vec::new(); nslots];
    for w in workers.into_iter() {
        // Local name id -> global name id.
        let mut map = vec![0u32; w.name_ids.len()];
        for (name, local_id) in w.name_ids.into_iter() {
            let global_id = *global_ids.entry(name).or_insert_with(|| {
                let g = next_global_id;
                next_global_id += 1;
                g
            });
            map[local_id as usize] = global_id;
        }

        for (slot, a) in w.aggs.iter().enumerate() {
            if a.count == 0 {
                continue;
            }
            let t = &mut totals[slot];
            t.count += a.count;
            t.id_sum += a.id_sum;
            t.amt_sum += a.amt_sum;
            t.amt_min = t.amt_min.min(a.amt_min);
            t.amt_max = t.amt_max.max(a.amt_max);
            t.name_bytes += a.name_bytes;
            t.note_bytes += a.note_bytes;
            for (word_idx, &word) in a.name_bits.iter().enumerate() {
                let mut bits = word;
                while bits != 0 {
                    let bit = bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    set_bit(&mut distinct_bits[slot], map[word_idx * 64 + bit]);
                }
            }
        }
    }

    // Output: countries sorted by code, then _UNMATCHED.
    let mut order: Vec<usize> = (0..codes.len()).filter(|&s| totals[s].count > 0).collect();
    order.sort_by_key(|&s| codes[s]);
    if totals[unmatched].count > 0 {
        order.push(unmatched);
    }

    let mut out: Vec<u8> = Vec::new();
    for slot in order {
        let t = &totals[slot];
        let key = if slot == unmatched {
            "_UNMATCHED".to_string()
        } else {
            String::from_utf8_lossy(&codes[slot]).into_owned()
        };
        let distinct: u32 = distinct_bits[slot].iter().map(|w| w.count_ones()).sum();
        writeln!(
            out,
            "{} {} {} {} {} {} {} {} {}",
            key,
            t.count,
            t.id_sum,
            money(t.amt_sum),
            money(t.amt_min),
            money(t.amt_max),
            distinct,
            t.name_bytes,
            t.note_bytes
        )?;
    }

    if let Some(dir) = Path::new(output_path).parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(output_path, out)
}