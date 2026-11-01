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
use std::hash::{BuildHasherDefault, Hasher};
use std::io::Write;
use std::path::Path;
use std::thread;

const K: u64 = 0x517cc1b727220a95;

#[derive(Default)]
struct FxHasher {
    hash: u64,
}

impl Hasher for FxHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }

    #[inline]
    fn write(&mut self, mut bytes: &[u8]) {
        while bytes.len() >= 8 {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&bytes[..8]);
            self.write_u64(u64::from_ne_bytes(buf));
            bytes = &bytes[8..];
        }
        if bytes.len() >= 4 {
            let mut buf = [0u8; 4];
            buf.copy_from_slice(&bytes[..4]);
            self.write_u32(u32::from_ne_bytes(buf));
            bytes = &bytes[4..];
        }
        for &b in bytes {
            self.write_u8(b);
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.hash = (self.hash.rotate_left(5) ^ (i as u64)).wrapping_mul(K);
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.hash = (self.hash.rotate_left(5) ^ (i as u64)).wrapping_mul(K);
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.hash = (self.hash.rotate_left(5) ^ i).wrapping_mul(K);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.hash = (self.hash.rotate_left(5) ^ (i as u64)).wrapping_mul(K);
    }
}

impl FxHasher {
    #[inline]
    fn hash(bytes: &[u8]) -> u64 {
        let mut hasher = FxHasher::default();
        hasher.write_usize(bytes.len());
        hasher.write(bytes);
        hasher.finish()
    }
}

type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

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
    /// Sparse local name IDs seen in this group by this thread.
    name_ids: Vec<u32>,
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
            name_ids: Vec::new(),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct NameSpan {
    /// Bit 31: 0 = Input buffer slice, 1 = Worker arena slice.
    /// Bits 0..30: Byte offset within the respective buffer.
    tagged_offset: u32,
    /// Byte length of the unescaped name.
    len: u32,
}

impl NameSpan {
    const ARENA_FLAG: u32 = 1 << 31;
    const OFFSET_MASK: u32 = !(1 << 31);

    #[inline]
    fn from_input(offset: usize, len: usize) -> Self {
        assert!(offset <= Self::OFFSET_MASK as usize, "Input offset exceeds 2 GB limit");
        assert!(len <= u32::MAX as usize, "Length exceeds u32::MAX");
        assert!(offset.checked_add(len).is_some(), "Offset + length arithmetic overflow");
        Self {
            tagged_offset: offset as u32,
            len: len as u32,
        }
    }

    #[inline]
    fn from_arena(offset: usize, len: usize) -> Self {
        assert!(offset <= Self::OFFSET_MASK as usize, "Arena offset exceeds 2 GB limit");
        assert!(len <= u32::MAX as usize, "Length exceeds u32::MAX");
        assert!(offset.checked_add(len).is_some(), "Offset + length arithmetic overflow");
        Self {
            tagged_offset: (offset as u32) | Self::ARENA_FLAG,
            len: len as u32,
        }
    }

    #[inline]
    fn is_in_arena(&self) -> bool {
        (self.tagged_offset & Self::ARENA_FLAG) != 0
    }

    #[inline]
    fn offset(&self) -> usize {
        (self.tagged_offset & Self::OFFSET_MASK) as usize
    }

    #[inline]
    fn len(&self) -> usize {
        self.len as usize
    }

    #[inline]
    fn resolve<'a>(&self, input: &'a [u8], arena: &'a [u8]) -> &'a [u8] {
        let off = self.offset();
        let l = self.len();
        let buf = if self.is_in_arena() { arena } else { input };
        let end = off.checked_add(l).expect("Slice bounds arithmetic overflow");
        assert!(end <= buf.len(), "Slice bounds exceed buffer length");
        &buf[off..end]
    }
}

struct NameTable {
    mask: usize,
    count: usize,
    slots: Vec<u32>,
}

impl NameTable {
    fn new(capacity: usize) -> Self {
        assert!(capacity > 0 && capacity.is_power_of_two(), "Capacity must be a power of two");
        NameTable {
            mask: capacity - 1,
            count: 0,
            slots: vec![0u32; capacity],
        }
    }

    #[inline]
    fn get_or_insert(
        &mut self,
        name: &[u8],
        is_escaped: bool,
        input_offset: usize,
        input: &[u8],
        arena: &mut Vec<u8>,
        names: &mut Vec<NameSpan>,
    ) -> u32 {
        let hash = FxHasher::hash(name);
        let mut idx = (hash as usize) & self.mask;
        loop {
            let slot = self.slots[idx];
            if slot == 0 {
                // Ensure load factor <= 70% before adding a new entry
                if (self.count + 1) * 10 > self.slots.len() * 7 {
                    self.resize(input, arena, names);
                    idx = (hash as usize) & self.mask;
                    while self.slots[idx] != 0 {
                        idx = (idx + 1) & self.mask;
                    }
                }
                let local_id = names.len() as u32;
                let span = if !is_escaped {
                    NameSpan::from_input(input_offset, name.len())
                } else {
                    let arena_offset = arena.len();
                    arena.extend_from_slice(name);
                    NameSpan::from_arena(arena_offset, name.len())
                };
                names.push(span);
                self.slots[idx] = local_id + 1;
                self.count += 1;
                return local_id;
            }
            let existing_id = slot - 1;
            let existing_span = &names[existing_id as usize];
            if existing_span.resolve(input, arena) == name {
                return existing_id;
            }
            idx = (idx + 1) & self.mask;
        }
    }

    fn resize(&mut self, input: &[u8], arena: &[u8], names: &[NameSpan]) {
        let new_size = self.slots.len().checked_mul(2).expect("Capacity overflow");
        let new_mask = new_size - 1;
        let mut new_slots = vec![0u32; new_size];
        for (lid, span) in names.iter().enumerate() {
            let bytes = span.resolve(input, arena);
            let hash = FxHasher::hash(bytes);
            let mut idx = (hash as usize) & new_mask;
            while new_slots[idx] != 0 {
                idx = (idx + 1) & new_mask;
            }
            new_slots[idx] = (lid as u32) + 1;
        }
        self.slots = new_slots;
        self.mask = new_mask;
    }
}

/// Everything one thread produces.
struct Worker {
    /// Slot index of _UNMATCHED (the last slot).
    unmatched: usize,
    aggs: Vec<Agg>,
    names: Vec<NameSpan>,
    arena: Vec<u8>,
    table: NameTable,
    /// Holds one unescaped name at a time.
    scratch: Vec<u8>,
}

impl Worker {
    fn new(nslots: usize) -> Self {
        Worker {
            unmatched: nslots - 1,
            aggs: vec![Agg::new(); nslots],
            names: Vec::with_capacity(4096),
            arena: Vec::with_capacity(32768),
            table: NameTable::new(8192),
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
    let (name, is_escaped, input_offset) = if npairs == 0 {
        (&d[ns..ne], false, ns)
    } else {
        unescape_into(&d[ns..ne], &mut w.scratch);
        (&w.scratch[..], true, 0)
    };
    let lid = w.table.get_or_insert(name, is_escaped, input_offset, d, &mut w.arena, &mut w.names);
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
    agg.name_ids.push(lid);
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
    for a in &mut w.aggs {
        if !a.name_ids.is_empty() {
            a.name_ids.sort_unstable();
            a.name_ids.dedup();
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

    // Move all worker arenas, name lists, and aggregates into separately owned storage before merging.
    let mut worker_arenas: Vec<Vec<u8>> = Vec::with_capacity(workers.len());
    let mut worker_names: Vec<Vec<NameSpan>> = Vec::with_capacity(workers.len());
    let mut worker_aggs: Vec<Vec<Agg>> = Vec::with_capacity(workers.len());
    for w in workers {
        worker_arenas.push(w.arena);
        worker_names.push(w.names);
        worker_aggs.push(w.aggs);
    }

    let mut totals: Vec<Agg> = vec![Agg::new(); nslots];
    let mut distinct_bits: Vec<Vec<u64>> = vec![Vec::new(); nslots];

    // Explicit scope ensures global_ids is dropped before its backing buffers
    // (data and worker_arenas).
    {
        let mut global_ids: FastMap<&[u8], u32> = FastMap::default();
        let mut next_global_id = 0u32;

        for w_idx in 0..worker_names.len() {
            let names = &worker_names[w_idx];
            let arena = &worker_arenas[w_idx];
            let aggs = &worker_aggs[w_idx];

            let mut map = vec![0u32; names.len()];
            for (local_id, span) in names.iter().enumerate() {
                let name_bytes: &[u8] = span.resolve(data, arena);
                let global_id = *global_ids.entry(name_bytes).or_insert_with(|| {
                    let g = next_global_id;
                    next_global_id += 1;
                    g
                });
                map[local_id] = global_id;
            }

            for (slot, a) in aggs.iter().enumerate() {
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
                for &local_id in &a.name_ids {
                    set_bit(&mut distinct_bits[slot], map[local_id as usize]);
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


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_names() {
        let mut table = NameTable::new(16);
        let mut arena = Vec::new();
        let mut names = Vec::new();
        let data = b"";

        // Unquoted empty
        let id1 = table.get_or_insert(b"", false, 0, data, &mut arena, &mut names);
        // Quoted empty
        let id2 = table.get_or_insert(b"", false, 0, data, &mut arena, &mut names);
        assert_eq!(id1, id2);
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].len(), 0);
        assert_eq!(names[0].resolve(data, &arena), b"");
    }

    #[test]
    fn test_trailing_spaces_and_escaped_quotes() {
        let mut table = NameTable::new(16);
        let mut arena = Vec::new();
        let mut names = Vec::new();
        let data = b"BettsBetts ";

        let id1 = table.get_or_insert(&data[0..5], false, 0, data, &mut arena, &mut names);
        let id2 = table.get_or_insert(&data[5..11], false, 5, data, &mut arena, &mut names);
        assert_ne!(id1, id2);
        assert_eq!(names[id1 as usize].resolve(data, &arena), b"Betts");
        assert_eq!(names[id2 as usize].resolve(data, &arena), b"Betts ");

        // Escaped quote
        let unescaped = b"Ann \"Annie\" Lee";
        let id3 = table.get_or_insert(unescaped, true, 0, data, &mut arena, &mut names);
        assert_ne!(id1, id3);
        assert_ne!(id2, id3);
        assert!(names[id3 as usize].is_in_arena());
        assert_eq!(names[id3 as usize].resolve(data, &arena), unescaped);

        // Same escaped quote seen again
        let id4 = table.get_or_insert(unescaped, true, 0, data, &mut arena, &mut names);
        assert_eq!(id3, id4);
        assert_eq!(arena.len(), unescaped.len()); // Arena should not duplicate
    }

    #[test]
    fn test_table_resizing_and_collisions() {
        let mut table = NameTable::new(16);
        let mut arena = Vec::new();
        let mut names = Vec::new();
        let mut synthetic_data = Vec::new();

        // Insert 1000 distinct names to trigger multiple resizes
        for i in 0..1000 {
            let s = format!("name_unique_{}", i);
            let offset = synthetic_data.len();
            synthetic_data.extend_from_slice(s.as_bytes());
            let id = table.get_or_insert(s.as_bytes(), false, offset, &synthetic_data, &mut arena, &mut names);
            assert_eq!(id, i as u32);
        }

        // Verify all 1000 names resolve accurately
        for i in 0..1000 {
            let s = format!("name_unique_{}", i);
            let resolved = names[i].resolve(&synthetic_data, &arena);
            assert_eq!(resolved, s.as_bytes());
        }

        assert_eq!(table.count, 1000);
        assert!(table.slots.len() >= 2048);
    }

    #[test]
    fn test_namespan_bounds_validation() {
        let span_input = NameSpan::from_input(100, 50);
        assert!(!span_input.is_in_arena());
        assert_eq!(span_input.offset(), 100);
        assert_eq!(span_input.len(), 50);

        let span_arena = NameSpan::from_arena(200, 25);
        assert!(span_arena.is_in_arena());
        assert_eq!(span_arena.offset(), 200);
        assert_eq!(span_arena.len(), 25);
    }
}
