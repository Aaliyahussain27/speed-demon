use std::collections::HashMap;
use std::env;
use std::hash::{BuildHasherDefault, Hasher};
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;

static THREAD_CAP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn hw_threads() -> usize {
    let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let c = THREAD_CAP.load(std::sync::atomic::Ordering::Relaxed);
    if c > 0 { n.min(c) } else { n }
}

#[repr(C)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

unsafe extern "C" {
    fn clock_gettime(clk_id: i32, tp: *mut Timespec) -> i32;
}

fn cpu_ns() -> u64 {
    let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { clock_gettime(2, &mut ts); }
    (ts.tv_sec as u64) * 1_000_000_000 + ts.tv_nsec as u64
}

const DOCS_FILE: &str = "docs.txt";
const QUERIES_FILE: &str = "queries.txt";
const RESULTS_FILE: &str = "results.txt";

#[derive(Default, Clone, Copy)]
struct FastHasher { h: u64 }

const MIX_K: u64 = 0x9E37_79B9_7F4A_7C15;

impl FastHasher {
    #[inline(always)]
    fn add(&mut self, w: u64) {
        self.h = (self.h.rotate_left(5) ^ w).wrapping_mul(MIX_K);
    }
}

impl Hasher for FastHasher {
    #[inline(always)]
    fn write(&mut self, bytes: &[u8]) {
        let mut b = bytes;
        while b.len() >= 8 {
            self.add(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]));
            b = &b[8..];
        }
        if !b.is_empty() {
            let mut buf = [0u8; 8];
            buf[..b.len()].copy_from_slice(b);
            self.add(u64::from_le_bytes(buf));
        }
    }
    #[inline(always)]
    fn write_usize(&mut self, n: usize) { self.add(n as u64); }
    #[inline(always)]
    fn write_u8(&mut self, n: u8) { self.add(n as u64); }
    #[inline(always)]
    fn finish(&self) -> u64 {
        let t = (self.h as u128).wrapping_mul(0xD6E8_FEB8_6659_FD93u128);
        (t as u64) ^ ((t >> 64) as u64)
    }
}

type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<FastHasher>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op { And, Or, Not }

#[derive(Clone, Copy)]
struct Query {
    phrase: bool,
    t_off: u32,
    t_len: u32,
    o_off: u32,
    g_off: u32,
    g_len: u32,
}

struct Queries {
    items: Vec<Query>,
    terms: Vec<u32>,
    ops: Vec<Op>,
    targets: Vec<u8>,
}

fn build_trans_table() -> [u8; 256] {
    let mut table = [b' '; 256];
    for b in b'0'..=b'9' { table[b as usize] = b; }
    for b in b'a'..=b'z' { table[b as usize] = b; }
    for b in b'A'..=b'Z' { table[b as usize] = b + 32; }
    table
}

#[inline(always)]
fn find_byte(hay: &[u8], needle: u8) -> Option<usize> {
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;
    let pat = LO.wrapping_mul(needle as u64);
    let mut base = 0usize;
    let mut chunks = hay.chunks_exact(8);
    for c in &mut chunks {
        let w = u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]) ^ pat;
        let z = w.wrapping_sub(LO) & !w & HI;
        if z != 0 { return Some(base + (z.trailing_zeros() >> 3) as usize); }
        base += 8;
    }
    for (i, &b) in chunks.remainder().iter().enumerate() {
        if b == needle { return Some(base + i); }
    }
    None
}

struct SplitByte<'a> {
    rest: &'a [u8],
    sep: u8,
    done: bool,
}

impl<'a> SplitByte<'a> {
    #[inline(always)]
    fn new(s: &'a [u8], sep: u8) -> Self { SplitByte { rest: s, sep, done: false } }
}

impl<'a> Iterator for SplitByte<'a> {
    type Item = &'a [u8];
    #[inline(always)]
    fn next(&mut self) -> Option<&'a [u8]> {
        if self.done { return None; }
        match find_byte(self.rest, self.sep) {
            Some(p) => {
                let seg = &self.rest[..p];
                self.rest = &self.rest[p + 1..];
                Some(seg)
            }
            None => {
                self.done = true;
                Some(self.rest)
            }
        }
    }
}

fn read_queries(path: &Path) -> Vec<u8> {
    let mut qf = File::open(path).expect("open queries.txt");
    let qlen = qf.metadata().map(|m| m.len() as usize).unwrap_or(0);
    let mut raw: Vec<u8> = Vec::with_capacity(qlen + 1);
    qf.read_to_end(&mut raw).expect("read queries.txt");
    raw
}

fn extract_queries(raw: &[u8]) -> (Queries, FastMap<Vec<u8>, u32>) {
    let mut items: Vec<Query> = Vec::with_capacity(raw.len() / 24 + 1);
    let mut terms: Vec<u32> = Vec::with_capacity(raw.len() / 6 + 1);
    let mut ops: Vec<Op> = Vec::with_capacity(raw.len() / 12 + 1);
    let mut targets: Vec<u8> = Vec::new();
    let mut term_to_id: FastMap<Vec<u8>, u32> = FastMap::with_capacity_and_hasher(1 << 16, Default::default());

    #[inline(always)]
    fn get_or_insert(map: &mut FastMap<Vec<u8>, u32>, term: &[u8]) -> u32 {
        if let Some(&id) = map.get(term) { id }
        else { let id = map.len() as u32; map.insert(term.to_vec(), id); id }
    }

    for line in SplitByte::new(raw, b'\n') {
        let line = if line.ends_with(b"\r") { &line[..line.len()-1] } else { line };
        if line.is_empty() { continue; }

        if let Some(pos) = find_byte(line, b' ') {
            let kind = &line[..pos];
            let rest = &line[pos+1..];

            if kind == b"BOOL" {
                let t_off = terms.len();
                let o_off = ops.len();
                let mut it = SplitByte::new(rest, b' ');
                let first = it.next().unwrap();
                terms.push(get_or_insert(&mut term_to_id, first));
                while let Some(op) = it.next() {
                    ops.push(match op {
                        b"AND" => Op::And, b"OR" => Op::Or, b"NOT" => Op::Not,
                        _ => panic!("Unknown op"),
                    });
                    let t = it.next().unwrap();
                    terms.push(get_or_insert(&mut term_to_id, t));
                }
                items.push(Query {
                    phrase: false,
                    t_off: t_off as u32,
                    t_len: (terms.len() - t_off) as u32,
                    o_off: o_off as u32,
                    g_off: 0,
                    g_len: 0,
                });
            } else if kind == b"PHRASE" {
                let inner = if rest.starts_with(b"\"") && rest.ends_with(b"\"") {
                    &rest[1..rest.len()-1]
                } else { rest };
                let t_off = terms.len();
                let g_off = targets.len();
                targets.push(b' ');
                for (idx, w) in SplitByte::new(inner, b' ').enumerate() {
                    if idx > 0 { targets.push(b' '); }
                    targets.extend_from_slice(w);
                    terms.push(get_or_insert(&mut term_to_id, w));
                }
                targets.push(b' ');
                items.push(Query {
                    phrase: true,
                    t_off: t_off as u32,
                    t_len: (terms.len() - t_off) as u32,
                    o_off: 0,
                    g_off: g_off as u32,
                    g_len: (targets.len() - g_off) as u32,
                });
            }
        }
    }
    (Queries { items, terms, ops, targets }, term_to_id)
}

fn extract_queries_parallel(raw: &[u8]) -> (Queries, FastMap<Vec<u8>, u32>) {
    let nt = hw_threads().min(4);
    if nt < 2 || raw.len() < (1 << 16) { return extract_queries(raw); }

    let mut bounds: Vec<usize> = vec![0];
    for i in 1..nt {
        let mut pos = (i * raw.len()) / nt;
        let prev = *bounds.last().unwrap();
        if pos < prev { pos = prev; }
        pos = match find_byte(&raw[pos..], b'\n') { Some(p) => pos + p + 1, None => raw.len() };
        bounds.push(pos);
    }
    bounds.push(raw.len());

    let parts: Vec<(Queries, FastMap<Vec<u8>, u32>)> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..nt).map(|i| {
            let c = &raw[bounds[i]..bounds[i + 1]];
            s.spawn(move || extract_queries(c))
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut it = parts.into_iter();
    let (q0, mut global) = it.next().unwrap();
    let Queries { mut items, mut terms, mut ops, mut targets } = q0;

    for (q, map) in it {
        let mut remap: Vec<u32> = vec![0; map.len()];
        for (term, lid) in map {
            let gid = match global.get(term.as_slice()) {
                Some(&g) => g,
                None => { let g = global.len() as u32; global.insert(term, g); g }
            };
            remap[lid as usize] = gid;
        }
        let t_base = terms.len() as u32;
        let o_base = ops.len() as u32;
        let g_base = targets.len() as u32;
        terms.extend(q.terms.iter().map(|&t| remap[t as usize]));
        ops.extend_from_slice(&q.ops);
        targets.extend_from_slice(&q.targets);
        items.extend(q.items.iter().map(|it| Query {
            t_off: it.t_off + t_base,
            o_off: it.o_off + o_base,
            g_off: it.g_off + g_base,
            ..*it
        }));
    }
    (Queries { items, terms, ops, targets }, global)
}

struct InvertedIndex {
    doc_ids: Vec<u32>,
    offsets: Vec<u32>,
}
impl InvertedIndex {
    #[inline(always)]
    fn get(&self, tid: u32) -> &[u32] {
        let s = self.offsets[tid as usize] as usize;
        let e = self.offsets[tid as usize + 1] as usize;
        &self.doc_ids[s..e]
    }
}

const NONE_TOK: u32 = u32::MAX;

struct DocStore {
    streams: Vec<Vec<u32>>,
    offs: Vec<Vec<u32>>,
    starts: Vec<u32>,
}
impl DocStore {
    #[inline(always)]
    fn has_phrase(&self, id: u32, ids: &[u32]) -> bool {
        let ci = self.starts.partition_point(|&s| s <= id) - 1;
        let l = (id - self.starts[ci]) as usize;
        let off = &self.offs[ci];
        let toks = &self.streams[ci][off[l] as usize..off[l + 1] as usize];
        let k = ids.len();
        if toks.len() < k { return false; }
        let first = ids[0];
        let last_start = toks.len() - k;
        let mut i = 0;
        while i <= last_start {
            if toks[i] == first && toks[i + 1..i + k] == ids[1..] { return true; }
            i += 1;
        }
        false
    }
}

const SLOT_EMPTY: u32 = u32::MAX;

#[derive(Clone, Copy)]
struct Slot {
    k0: u64,
    k1: u64,
    id: u32,
    len: u32,
}

struct TermTable {
    slots: Vec<Slot>,
    mask: usize,
    long: FastMap<Vec<u8>, u32>,
    n: usize,
}

#[inline(always)]
fn pack_key(b: &[u8]) -> (u64, u64) {
    let mut buf = [0u8; 16];
    buf[..b.len()].copy_from_slice(b);
    let v = u128::from_le_bytes(buf);
    (v as u64, (v >> 64) as u64)
}

#[inline(always)]
fn hash_key(k0: u64, k1: u64) -> u64 {
    let x = (k0 ^ k1.rotate_left(29).wrapping_mul(0xA24B_AED4_963E_E407)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let t = (x as u128).wrapping_mul(0xD6E8_FEB8_6659_FD93u128);
    (t as u64) ^ ((t >> 64) as u64)
}

impl TermTable {
    fn from_map(map: &FastMap<Vec<u8>, u32>) -> TermTable {
        let n = map.len();
        let cap = (n * 2).max(16).next_power_of_two();
        let mask = cap - 1;
        let mut slots = vec![Slot { k0: 0, k1: 0, id: SLOT_EMPTY, len: 0 }; cap];
        let mut long: FastMap<Vec<u8>, u32> = FastMap::default();
        for (k, &id) in map.iter() {
            if k.len() <= 16 {
                let (k0, k1) = pack_key(k);
                let mut i = (hash_key(k0, k1) as usize) & mask;
                while slots[i].id != SLOT_EMPTY { i = (i + 1) & mask; }
                slots[i] = Slot { k0, k1, id, len: k.len() as u32 };
            } else {
                long.insert(k.clone(), id);
            }
        }
        TermTable { slots, mask, long, n }
    }

    #[inline(always)]
    fn len(&self) -> usize { self.n }

    #[inline(always)]
    fn get(&self, tok: &[u8]) -> Option<u32> {
        let n = tok.len();
        if n <= 16 {
            let (k0, k1) = pack_key(tok);
            let mut i = (hash_key(k0, k1) as usize) & self.mask;
            loop {
                let s = &self.slots[i];
                if s.id == SLOT_EMPTY { return None; }
                if s.k0 == k0 && s.k1 == k1 && s.len as usize == n { return Some(s.id); }
                i = (i + 1) & self.mask;
            }
        } else {
            self.long.get(tok).copied()
        }
    }
}

#[inline(always)]
fn for_each_token<F: FnMut(&[u8])>(text: &[u8], trans: &[u8; 256], tok: &mut Vec<u8>, mut f: F) {
    for &b in text {
        let norm = trans[b as usize];
        if norm != b' ' {
            tok.push(norm);
        } else if !tok.is_empty() {
            f(&tok[..]);
            tok.clear();
        }
    }
    if !tok.is_empty() {
        f(&tok[..]);
        tok.clear();
    }
}

#[derive(Clone, Copy)]
struct SharedOut(*mut u32, usize);
unsafe impl Send for SharedOut {}
unsafe impl Sync for SharedOut {}
impl SharedOut {
    #[inline(always)]
    fn write(&self, i: usize, v: u32) {
        assert!(i < self.1);
        unsafe { *self.0.add(i) = v; }
    }
}

#[cfg(unix)]
fn read_docs(path: &Path) -> Vec<u8> {
    use std::os::unix::fs::FileExt;
    let f = File::open(path).expect("open docs.txt");
    let len = f.metadata().expect("stat docs.txt").len() as usize;
    let mut raw: Vec<u8> = vec![0u8; len];
    if len > 0 {
        let readers = hw_threads().min(4);
        let per = ((len + readers - 1) / readers).max(1);
        std::thread::scope(|s| {
            for (i, chunk) in raw.chunks_mut(per).enumerate() {
                let f = &f;
                s.spawn(move || {
                    f.read_exact_at(chunk, (i * per) as u64).expect("read docs.txt");
                });
            }
        });
    }
    raw
}

#[cfg(not(unix))]
fn read_docs(path: &Path) -> Vec<u8> {
    let mut f = File::open(path).expect("open docs.txt");
    let len = f.metadata().map(|m| m.len() as usize).unwrap_or(0);
    let mut raw: Vec<u8> = Vec::with_capacity(len + 1);
    f.read_to_end(&mut raw).expect("read docs.txt");
    raw
}

fn load_corpus(raw: Vec<u8>, term_ids: &TermTable) -> (DocStore, InvertedIndex) {

    let trans_table = build_trans_table();
    let nthreads = hw_threads().min(8);
    let n_terms = term_ids.len();

    let mut byte_starts: Vec<usize> = vec![0];
    for i in 1..nthreads {
        let mut pos = (i * raw.len()) / nthreads;
        while pos < raw.len() && raw[pos] != b'\n' { pos += 1; }
        if pos < raw.len() { pos += 1; }
        byte_starts.push(pos);
    }
    byte_starts.push(raw.len());

    assert!(raw.len() < u32::MAX as usize);

    type PassA = (Vec<u32>, Vec<u32>, Vec<u32>);

    let pass_a: Vec<PassA> = std::thread::scope(|s| {
        let raw_ref      = &raw[..];
        let term_ids_ref = term_ids;
        let mut handles = Vec::with_capacity(nthreads);

        for i in 0..nthreads {
            let bstart = byte_starts[i];
            let bend   = byte_starts[i + 1];

            handles.push(s.spawn(move || {
                let chunk = &raw_ref[bstart..bend];
                let mut counts: Vec<u32> = vec![0; n_terms];
                let mut last: Vec<u32> = vec![u32::MAX; n_terms];
                let mut stream: Vec<u32> = Vec::with_capacity(chunk.len() / 6 + 16);
                let mut offs: Vec<u32> = Vec::with_capacity(chunk.len() / 200 + 16);
                let mut tok: Vec<u8> = Vec::with_capacity(64);
                let mut doc_id: u32 = 0;
                offs.push(0);

                for line in chunk.split(|&b| b == b'\n') {
                    let line = if line.ends_with(b"\r") { &line[..line.len()-1] } else { line };
                    if line.is_empty() { continue; }

                    let ts = match line.iter().position(|&b| b == b'\t') {
                        Some(t) => t + 1,
                        None    => line.len(),
                    };
                    let text = &line[ts..];

                    let mut prev_none = false;
                    for_each_token(text, &trans_table, &mut tok, |t| {
                        match term_ids_ref.get(t) {
                            Some(tid) => {
                                prev_none = false;
                                stream.push(tid);
                                let u = tid as usize;
                                if last[u] != doc_id { last[u] = doc_id; counts[u] += 1; }
                            }
                            None => {
                                if !prev_none { stream.push(NONE_TOK); prev_none = true; }
                            }
                        }
                    });

                    offs.push(stream.len() as u32);
                    doc_id += 1;
                }
                (counts, stream, offs)
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    drop(raw);

    let mut streams: Vec<Vec<u32>> = Vec::with_capacity(nthreads);
    let mut offs_all: Vec<Vec<u32>> = Vec::with_capacity(nthreads);
    let mut counts_all: Vec<Vec<u32>> = Vec::with_capacity(nthreads);
    for (counts, stream, offs) in pass_a {
        counts_all.push(counts);
        streams.push(stream);
        offs_all.push(offs);
    }

    let mut doc_id_starts: Vec<u32> = Vec::with_capacity(nthreads + 1);
    let mut cumulative: u32 = 0;
    doc_id_starts.push(0);
    for offs in &offs_all {
        cumulative += (offs.len() - 1) as u32;
        doc_id_starts.push(cumulative);
    }

    let mut inv_offsets: Vec<u32> = Vec::with_capacity(n_terms + 1);
    let mut total: u32 = 0;
    inv_offsets.push(0);
    for t in 0..n_terms {
        let mut c: u32 = 0;
        for counts in &counts_all { c += counts[t]; }
        total += c;
        inv_offsets.push(total);
    }

    let mut running: Vec<u32> = inv_offsets[..n_terms].to_vec();
    let mut cursors: Vec<Vec<u32>> = Vec::with_capacity(nthreads);
    for counts in &counts_all {
        cursors.push(running.clone());
        for t in 0..n_terms { running[t] += counts[t]; }
    }
    drop(counts_all);

    let mut doc_ids: Vec<u32> = vec![0u32; total as usize];
    let out = SharedOut(doc_ids.as_mut_ptr(), doc_ids.len());

    std::thread::scope(|s| {
        for (i, cursor) in cursors.iter_mut().enumerate() {
            let stream = &streams[i];
            let offs   = &offs_all[i];
            let base   = doc_id_starts[i];

            s.spawn(move || {
                let mut last: Vec<u32> = vec![u32::MAX; n_terms];
                let nd = offs.len() - 1;

                for local in 0..nd {
                    let l = local as u32;
                    let gid = base + l;
                    for &tid in &stream[offs[local] as usize..offs[local + 1] as usize] {
                        if tid == NONE_TOK { continue; }
                        let u = tid as usize;
                        if last[u] != l {
                            last[u] = l;
                            out.write(cursor[u] as usize, gid);
                            cursor[u] += 1;
                        }
                    }
                }
            });
        }
    });

    (DocStore { streams, offs: offs_all, starts: doc_id_starts },
     InvertedIndex { doc_ids, offsets: inv_offsets })
}

#[inline(always)]
fn gallop(slice: &[u32], target: u32) -> usize {
    let n = slice.len();
    if n == 0 || slice[0] >= target { return 0; }
    let mut step = 1; let mut prev = 0;
    while step < n && slice[step] < target { prev = step; step <<= 1; }
    let high = step.min(n);
    let mut low = prev; let mut len = high - low;
    while len > 0 {
        let half = len / 2; let mid = low + half;
        if slice[mid] < target { low = mid+1; len -= half+1; } else { len = half; }
    }
    low
}

fn intersect(mut a: &[u32], mut b: &[u32], out: &mut Vec<u32>) {
    out.clear(); out.reserve(a.len().min(b.len()));
    while !a.is_empty() && !b.is_empty() {
        let va = a[0]; let vb = b[0];
        if      va == vb { out.push(va); a = &a[1..]; b = &b[1..]; }
        else if va <  vb { if a.len() > 16*b.len() { a = &a[gallop(a,vb)..]; } else { a = &a[1..]; } }
        else             { if b.len() > 16*a.len() { b = &b[gallop(b,va)..]; } else { b = &b[1..]; } }
    }
}

fn union(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear(); out.reserve(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if      a[i] == b[j] { out.push(a[i]); i+=1; j+=1; }
        else if a[i] <  b[j] { out.push(a[i]); i+=1; }
        else                  { out.push(b[j]); j+=1; }
    }
    if i < a.len() { out.extend_from_slice(&a[i..]); }
    else if j < b.len() { out.extend_from_slice(&b[j..]); }
}

fn difference(mut a: &[u32], mut b: &[u32], out: &mut Vec<u32>) {
    out.clear(); out.reserve(a.len());
    while !a.is_empty() && !b.is_empty() {
        let va = a[0]; let vb = b[0];
        if      va == vb { a = &a[1..]; b = &b[1..]; }
        else if va <  vb { out.push(va); a = &a[1..]; }
        else             { if b.len() > 16*a.len() { b = &b[gallop(b,va)..]; } else { b = &b[1..]; } }
    }
    if !a.is_empty() { out.extend_from_slice(a); }
}

#[inline(always)]
fn apply(op: Op, a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    match op {
        Op::And => intersect(a, b, out),
        Op::Or  => union(a, b, out),
        Op::Not => difference(a, b, out),
    }
}

#[inline(always)]
fn step(state: &mut u8, base: &[u32], op: Op, p: &[u32], buf_a: &mut Vec<u32>, buf_b: &mut Vec<u32>) {
    match *state {
        0 => { apply(op, base, p, buf_a); *state = 1; }
        1 => { apply(op, &buf_a[..], p, buf_b); *state = 2; }
        _ => { apply(op, &buf_b[..], p, buf_a); *state = 1; }
    }
}

#[inline(always)]
fn cur_len(state: u8, base: &[u32], buf_a: &Vec<u32>, buf_b: &Vec<u32>) -> usize {
    match state { 0 => base.len(), 1 => buf_a.len(), _ => buf_b.len() }
}

fn run_bool<'a>(
    index: &'a InvertedIndex, terms: &[u32], ops: &[Op],
    buf_a: &'a mut Vec<u32>, buf_b: &'a mut Vec<u32>, scratch: &mut Vec<(usize, u32)>,
) -> &'a [u32] {
    let n = ops.len();
    if n == 0 { return index.get(terms[0]); }

    let f = ops.iter().position(|&o| o == Op::Or).unwrap_or(n);

    scratch.clear();
    scratch.push((index.get(terms[0]).len(), terms[0]));
    for j in 0..f {
        if ops[j] == Op::And {
            let t = terms[j + 1];
            scratch.push((index.get(t).len(), t));
        }
    }
    scratch.sort_unstable();

    let mut state: u8 = 0;
    let base: &[u32] = index.get(scratch[0].1);
    for idx in 1..scratch.len() {
        if cur_len(state, base, buf_a, buf_b) == 0 { break; }
        step(&mut state, base, Op::And, index.get(scratch[idx].1), buf_a, buf_b);
    }
    for j in 0..f {
        if ops[j] == Op::Not {
            if cur_len(state, base, buf_a, buf_b) == 0 { break; }
            step(&mut state, base, Op::Not, index.get(terms[j + 1]), buf_a, buf_b);
        }
    }

    let mut k = f;
    while k < n {
        step(&mut state, base, Op::Or, index.get(terms[k + 1]), buf_a, buf_b);
        k += 1;
        let mut e = k;
        while e < n && ops[e] != Op::Or { e += 1; }
        if e > k {
            scratch.clear();
            for j in k..e {
                if ops[j] == Op::And {
                    let t = terms[j + 1];
                    scratch.push((index.get(t).len(), t));
                }
            }
            scratch.sort_unstable();
            for idx in 0..scratch.len() {
                if cur_len(state, base, buf_a, buf_b) == 0 { break; }
                step(&mut state, base, Op::And, index.get(scratch[idx].1), buf_a, buf_b);
            }
            for j in k..e {
                if ops[j] == Op::Not {
                    if cur_len(state, base, buf_a, buf_b) == 0 { break; }
                    step(&mut state, base, Op::Not, index.get(terms[j + 1]), buf_a, buf_b);
                }
            }
            k = e;
        }
    }

    match state { 0 => base, 1 => &buf_a[..], _ => &buf_b[..] }
}

fn run_phrase(
    index: &InvertedIndex, doc_store: &DocStore, terms: &[u32],
    buf_a: &mut Vec<u32>, buf_b: &mut Vec<u32>, matches: &mut Vec<u32>,
    order: &mut Vec<u32>,
) -> (usize, u64) {
    matches.clear();
    order.clear();
    order.extend_from_slice(terms);
    order.sort_unstable_by_key(|&x| index.get(x).len());
    let first = index.get(order[0]);
    if first.is_empty() { return (0, 0); }
    buf_a.clear(); buf_a.extend_from_slice(first);
    let mut in_a = true;
    for &tid in &order[1..] {
        let p = index.get(tid);
        if in_a { if buf_a.is_empty() { return (0, 0); } intersect(buf_a, p, buf_b); in_a=false; }
        else    { if buf_b.is_empty() { return (0, 0); } intersect(buf_b, p, buf_a); in_a=true;  }
    }
    let candidates = if in_a { buf_a.as_slice() } else { buf_b.as_slice() };
    if candidates.is_empty() { return (0, 0); }
    let tv = std::time::Instant::now();
    for &doc_id in candidates {
        if doc_store.has_phrase(doc_id, terms) { matches.push(doc_id); }
    }
    (candidates.len(), tv.elapsed().as_nanos() as u64)
}

fn format_result<W: Write>(writer: &mut W, ids: &[u32]) {
    if ids.is_empty() { writeln!(writer, "0 0").unwrap(); return; }
    let count = ids.len();
    let sum: u64 = ids.iter().map(|&x| x as u64).sum();
    write!(writer, "{} {}", count, sum).unwrap();
    for &id in &ids[..count.min(10)] { write!(writer, " {}", id).unwrap(); }
    writeln!(writer).unwrap();
}

fn peak_rss_mb() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for l in s.lines() {
        if let Some(r) = l.strip_prefix("VmHWM:") {
            return r.trim().trim_end_matches("kB").trim().parse::<u64>().unwrap_or(0) / 1024;
        }
    }
    0
}

fn cpu_seconds() -> f64 {
    let s = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let after = match s.rfind(')') { Some(p) => &s[p + 1..], None => return 0.0 };
    let f: Vec<&str> = after.split_whitespace().collect();
    if f.len() < 13 { return 0.0; }
    let ut: f64 = f[11].parse().unwrap_or(0.0);
    let st: f64 = f[12].parse().unwrap_or(0.0);
    (ut + st) / 100.0
}

fn vm_peak_mb() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for l in s.lines() {
        if let Some(r) = l.strip_prefix("VmPeak:") {
            return r.trim().trim_end_matches("kB").trim().parse::<u64>().unwrap_or(0) / 1024;
        }
    }
    0
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut data_dir = "/data".to_string();
    let mut out_dir  = "/out".to_string();
    let mut i = 1;
    while i < args.len() {
        if      args[i] == "--data" && i+1 < args.len() { data_dir = args[i+1].clone(); i += 2; }
        else if args[i] == "--out"  && i+1 < args.len() { out_dir  = args[i+1].clone(); i += 2; }
        else { i += 1; }
    }

    let data_path = Path::new(&data_dir);
    if let Ok(txt) = std::fs::read_to_string(Path::new(&data_dir).join("threads.txt")) {
        if let Ok(v) = txt.trim().parse::<usize>() {
            THREAD_CAP.store(v, std::sync::atomic::Ordering::Relaxed);
        }
    }
    let out_path  = Path::new(&out_dir);

    let c0 = cpu_ns();
    let t0 = std::time::Instant::now();
    let qraw = read_queries(&data_path.join(QUERIES_FILE));
    let t_qread = t0.elapsed();

    let docs_path = data_path.join(DOCS_FILE);
    let reader = std::thread::spawn(move || read_docs(&docs_path));

    let (queries, term_to_id) = extract_queries_parallel(&qraw);
    let t_queries = t0.elapsed();
    let c1 = cpu_ns();

    let t1 = std::time::Instant::now();
    let raw = reader.join().expect("docs reader");
    let t_wait = t1.elapsed();

    let t2 = std::time::Instant::now();
    let term_table = TermTable::from_map(&term_to_id);
    let (doc_store, index) = load_corpus(raw, &term_table);
    let t_corpus = t2.elapsed();
    let c2 = cpu_ns();

    std::fs::create_dir_all(out_path).expect("create out dir");
    let out_file = File::create(out_path.join(RESULTS_FILE)).expect("create results.txt");
    let mut writer = BufWriter::new(out_file);

    let nthreads = hw_threads().min(8);
    let chunk_size = (queries.items.len() + nthreads - 1) / nthreads;
    let query_chunks: Vec<&[Query]> = queries.items.chunks(chunk_size).collect();
    let terms_ref: &[u32] = &queries.terms;
    let ops_ref: &[Op] = &queries.ops;

    let t_eval = std::time::Instant::now();
    let index_ref     = &index;
    let doc_store_ref = &doc_store;

    let rendered: Vec<(Vec<u8>, [u64; 8])> = std::thread::scope(|s| {
        let handles: Vec<_> = query_chunks.into_iter().map(|chunk| {
            s.spawn(move || {
                let mut out = Vec::with_capacity(chunk.len() * 32);
                let mut buf_a: Vec<u32> = Vec::new();
                let mut buf_b: Vec<u32> = Vec::new();
                let mut pm:    Vec<u32> = Vec::new();
                let mut order: Vec<u32> = Vec::new();
                let mut scratch: Vec<(usize, u32)> = Vec::new();
                let mut st = [0u64; 8];
                for q in chunk {
                    let tq = std::time::Instant::now();
                    let terms = &terms_ref[q.t_off as usize..(q.t_off + q.t_len) as usize];
                    if q.phrase {
                        let (cand, vns) = run_phrase(index_ref, doc_store_ref, terms, &mut buf_a, &mut buf_b, &mut pm, &mut order);
                        format_result(&mut out, &pm);
                        st[1] += tq.elapsed().as_nanos() as u64;
                        st[3] += 1;
                        st[4] += cand as u64;
                        st[7] += vns;
                    } else {
                        let ops = &ops_ref[q.o_off as usize..(q.o_off + q.t_len - 1) as usize];
                        let r = run_bool(index_ref, terms, ops, &mut buf_a, &mut buf_b, &mut scratch);
                        format_result(&mut out, r);
                        st[0] += tq.elapsed().as_nanos() as u64;
                        st[2] += 1;
                        st[5] += terms.iter().map(|&t| index_ref.get(t).len() as u64).sum::<u64>();
                        st[6] += r.len() as u64;
                    }
                }
                (out, st)
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let eval_dur = t_eval.elapsed();
    let c3 = cpu_ns();
    let t_write = std::time::Instant::now();
    let mut tot = [0u64; 8];
    for (buf, st) in rendered {
        for k in 0..8 { tot[k] += st[k]; }
        writer.write_all(&buf).expect("write");
    }
    writer.flush().expect("flush");
    let write_dur = t_write.elapsed();
    eprintln!("[PROFILE5] BOOL: {} queries, {} thread-ms, input elems {}, result elems {} | PHRASE: {} queries, {} thread-ms (verify {} thread-ms), candidates {} | results write: {:?}",
        tot[2], tot[0] / 1_000_000, tot[5], tot[6], tot[3], tot[1] / 1_000_000, tot[7] / 1_000_000, tot[4], write_dur);

    eprintln!("[PROFILE2] Queries file read: {:?} | Distinct terms: {} | Queries: {}", t_qread, term_to_id.len(), queries.items.len());
    eprintln!("[PROFILE] Queries parse: {:?} | Docs read wait: {:?} | Corpus index: {:?} | Parallel query eval: {:?}",
        t_queries, t_wait, t_corpus, eval_dur);
    eprintln!("[PROFILE3] Peak RSS: {} MB", peak_rss_mb());
    eprintln!("[PROFILE7] CPU ms: queries+docs read {} | corpus {} | eval {} | total {} | threads {}",
        (c1 - c0) / 1_000_000, (c2 - c1) / 1_000_000, (c3 - c2) / 1_000_000, (c3 - c0) / 1_000_000, hw_threads());
    eprintln!("[PROFILE6] CPU time user+sys: {:.2} s | VmPeak: {} MB | cpus visible: {}",
        cpu_seconds(), vm_peak_mb(), hw_threads());
}