use std::collections::HashMap;
use std::env;
use std::hash::{BuildHasherDefault, Hasher};
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;

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
}

struct Queries {
    items: Vec<Query>,
    terms: Vec<u32>,
    ops: Vec<Op>,
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

fn extract_queries(raw: &[u8]) -> (Queries, TermTable) {
    let mut items: Vec<Query> = Vec::with_capacity(raw.len() / 24 + 1);
    let mut terms: Vec<u32> = Vec::with_capacity(raw.len() / 6 + 1);
    let mut ops: Vec<Op> = Vec::with_capacity(raw.len() / 12 + 1);
    let mut table = TermTable::new();

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
                terms.push(table.intern(first));
                while let Some(op) = it.next() {
                    ops.push(match op {
                        b"AND" => Op::And, b"OR" => Op::Or, b"NOT" => Op::Not,
                        _ => panic!("Unknown op"),
                    });
                    let t = it.next().unwrap();
                    terms.push(table.intern(t));
                }
                items.push(Query {
                    phrase: false,
                    t_off: t_off as u32,
                    t_len: (terms.len() - t_off) as u32,
                    o_off: o_off as u32,
                });
            } else if kind == b"PHRASE" {
                let inner = if rest.starts_with(b"\"") && rest.ends_with(b"\"") {
                    &rest[1..rest.len()-1]
                } else { rest };
                let t_off = terms.len();
                for w in SplitByte::new(inner, b' ') {
                    terms.push(table.intern(w));
                }
                items.push(Query {
                    phrase: true,
                    t_off: t_off as u32,
                    t_len: (terms.len() - t_off) as u32,
                    o_off: 0,
                });
            }
        }
    }
    (Queries { items, terms, ops }, table)
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
    stream: Vec<u32>,
    offs: Vec<u32>,
}
impl DocStore {
    #[inline(always)]
    fn has_phrase(&self, id: u32, ids: &[u32]) -> bool {
        let l = id as usize;
        let toks = &self.stream[self.offs[l] as usize..self.offs[l + 1] as usize];
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

struct TermTable {
    ent: Vec<u64>,
    keys: Vec<(u64, u64)>,
    mask: usize,
    used: usize,
    long: FastMap<Vec<u8>, u32>,
    n: usize,
}

#[inline(always)]
fn load_le(b: &[u8]) -> u64 {
    let mut v = 0u64;
    for (i, &x) in b.iter().enumerate() { v |= (x as u64) << (8 * i); }
    v
}

#[inline(always)]
fn pack_key(b: &[u8]) -> (u64, u64) {
    if b.len() <= 8 { (load_le(b), 0) } else { (load_le(&b[..8]), load_le(&b[8..])) }
}

#[inline(always)]
fn hash_key(k0: u64, k1: u64) -> u64 {
    let x = (k0 ^ k1.rotate_left(29).wrapping_mul(0xA24B_AED4_963E_E407)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let t = (x as u128).wrapping_mul(0xD6E8_FEB8_6659_FD93u128);
    (t as u64) ^ ((t >> 64) as u64)
}

impl TermTable {
    fn new() -> TermTable {
        let cap = 1usize << 18;
        TermTable {
            ent: vec![0u64; cap],
            keys: Vec::with_capacity(1 << 17),
            mask: cap - 1,
            used: 0,
            long: FastMap::default(),
            n: 0,
        }
    }

    #[inline(always)]
    fn len(&self) -> usize { self.n }

    fn grow(&mut self) {
        let cap = self.ent.len() * 2;
        let mask = cap - 1;
        let mut ent = vec![0u64; cap];
        for &e in self.ent.iter() {
            if e == 0 { continue; }
            let id = (e as u32) - 1;
            let (k0, k1) = self.keys[id as usize];
            let mut i = (hash_key(k0, k1) as usize) & mask;
            while ent[i] != 0 { i = (i + 1) & mask; }
            ent[i] = e;
        }
        self.ent = ent;
        self.mask = mask;
    }

    fn intern(&mut self, term: &[u8]) -> u32 {
        if term.len() <= 16 && !term.iter().any(|&b| b == 0) {
            let (k0, k1) = pack_key(term);
            let h = hash_key(k0, k1);
            let tag = (h >> 32) as u32;
            let mut i = (h as usize) & self.mask;
            loop {
                let e = self.ent[i];
                if e == 0 { break; }
                if (e >> 32) as u32 == tag {
                    let id = (e as u32) - 1;
                    if self.keys[id as usize] == (k0, k1) { return id; }
                }
                i = (i + 1) & self.mask;
            }
            let id = self.n as u32;
            self.n += 1;
            self.keys.push((k0, k1));
            self.ent[i] = ((tag as u64) << 32) | (id as u64 + 1);
            self.used += 1;
            if self.used * 2 > self.ent.len() { self.grow(); }
            id
        } else {
            if let Some(&id) = self.long.get(term) { return id; }
            let id = self.n as u32;
            self.n += 1;
            self.keys.push((0, 0));
            self.long.insert(term.to_vec(), id);
            id
        }
    }

    #[inline(always)]
    fn get(&self, tok: &[u8]) -> Option<u32> {
        if tok.len() <= 16 {
            let (k0, k1) = pack_key(tok);
            let h = hash_key(k0, k1);
            let tag = (h >> 32) as u32;
            let mut i = (h as usize) & self.mask;
            loop {
                let e = self.ent[i];
                if e == 0 { return None; }
                if (e >> 32) as u32 == tag {
                    let id = (e as u32) - 1;
                    if self.keys[id as usize] == (k0, k1) { return Some(id); }
                }
                i = (i + 1) & self.mask;
            }
        } else {
            self.long.get(tok).copied()
        }
    }
}

#[inline(always)]
fn for_each_token<F: FnMut(&[u8])>(text: &[u8], trans: &[u8; 256], tok: &mut Vec<u8>, mut f: F) {
    let n = text.len();
    let mut i = 0;
    while i < n {
        while i < n && trans[text[i] as usize] == b' ' { i += 1; }
        if i >= n { break; }
        let start = i;
        let mut dirty = false;
        while i < n {
            let t = trans[text[i] as usize];
            if t == b' ' { break; }
            dirty |= t != text[i];
            i += 1;
        }
        if !dirty {
            f(&text[start..i]);
        } else {
            tok.clear();
            for &b in &text[start..i] { tok.push(trans[b as usize]); }
            f(&tok[..]);
        }
    }
}

fn load_corpus(raw: Vec<u8>, term_ids: &TermTable) -> (DocStore, InvertedIndex) {
    let trans_table = build_trans_table();
    let n_terms = term_ids.len();
    assert!(raw.len() < u32::MAX as usize);

    let mut st: Vec<[u32; 2]> = vec![[u32::MAX, 0]; n_terms];
    let mut stream: Vec<u32> = Vec::with_capacity(raw.len() / 6 + 16);
    let mut offs: Vec<u32> = Vec::with_capacity(raw.len() / 200 + 16);
    let mut tok: Vec<u8> = Vec::with_capacity(64);
    let mut doc_id: u32 = 0;
    offs.push(0);

    for line in raw.split(|&b| b == b'\n') {
        let line = if line.ends_with(b"\r") { &line[..line.len()-1] } else { line };
        if line.is_empty() { continue; }

        let ts = match line.iter().position(|&b| b == b'\t') {
            Some(t) => t + 1,
            None    => line.len(),
        };
        let text = &line[ts..];

        let mut prev_none = false;
        for_each_token(text, &trans_table, &mut tok, |t| {
            match term_ids.get(t) {
                Some(tid) => {
                    prev_none = false;
                    stream.push(tid);
                    let e = &mut st[tid as usize];
                    if e[0] != doc_id { e[0] = doc_id; e[1] += 1; }
                }
                None => {
                    if !prev_none { stream.push(NONE_TOK); prev_none = true; }
                }
            }
        });

        offs.push(stream.len() as u32);
        doc_id += 1;
    }
    drop(raw);

    let mut inv_offsets: Vec<u32> = Vec::with_capacity(n_terms + 1);
    let mut total: u32 = 0;
    inv_offsets.push(0);
    for t in 0..n_terms {
        total += st[t][1];
        inv_offsets.push(total);
    }

    let mut doc_ids: Vec<u32> = vec![0u32; total as usize];
    for t in 0..n_terms { st[t] = [u32::MAX, inv_offsets[t]]; }

    for local in 0..doc_id as usize {
        let l = local as u32;
        for &tid in &stream[offs[local] as usize..offs[local + 1] as usize] {
            if tid == NONE_TOK { continue; }
            let e = &mut st[tid as usize];
            if e[0] != l {
                e[0] = l;
                doc_ids[e[1] as usize] = l;
                e[1] += 1;
            }
        }
    }

    (DocStore { stream, offs }, InvertedIndex { doc_ids, offsets: inv_offsets })
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
) {
    matches.clear();
    order.clear();
    order.extend_from_slice(terms);
    order.sort_unstable_by_key(|&x| index.get(x).len());
    let first = index.get(order[0]);
    if first.is_empty() { return; }
    buf_a.clear(); buf_a.extend_from_slice(first);
    let mut in_a = true;
    for &tid in &order[1..] {
        let p = index.get(tid);
        if in_a { if buf_a.is_empty() { return; } intersect(buf_a, p, buf_b); in_a=false; }
        else    { if buf_b.is_empty() { return; } intersect(buf_b, p, buf_a); in_a=true;  }
    }
    let candidates = if in_a { buf_a.as_slice() } else { buf_b.as_slice() };
    for &doc_id in candidates {
        if doc_store.has_phrase(doc_id, terms) { matches.push(doc_id); }
    }
}

#[inline(always)]
fn push_num(buf: &mut Vec<u8>, mut v: u64) {
    if v == 0 { buf.push(b'0'); return; }
    let mut tmp = [0u8; 20];
    let mut i = 20;
    while v > 0 {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    buf.extend_from_slice(&tmp[i..]);
}

fn format_result<W: Write>(writer: &mut W, line: &mut Vec<u8>, ids: &[u32]) {
    line.clear();
    if ids.is_empty() {
        line.extend_from_slice(b"0 0\n");
    } else {
        let count = ids.len();
        let sum: u64 = ids.iter().map(|&x| x as u64).sum();
        push_num(line, count as u64);
        line.push(b' ');
        push_num(line, sum);
        for &id in &ids[..count.min(10)] {
            line.push(b' ');
            push_num(line, id as u64);
        }
        line.push(b'\n');
    }
    writer.write_all(line).unwrap();
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
    let out_path  = Path::new(&out_dir);

    let c0 = cpu_ns();
    let t0 = std::time::Instant::now();
    let qraw = read_queries(&data_path.join(QUERIES_FILE));
    let t_qread = t0.elapsed();
    let (queries, term_table) = extract_queries(&qraw);
    let t_queries = t0.elapsed();
    let c1 = cpu_ns();

    let t1 = std::time::Instant::now();
    let raw = std::fs::read(data_path.join(DOCS_FILE)).expect("read docs.txt");
    let t_read = t1.elapsed();

    let t2 = std::time::Instant::now();
    let (doc_store, index) = load_corpus(raw, &term_table);
    let t_corpus = t2.elapsed();
    let c2 = cpu_ns();

    std::fs::create_dir_all(out_path).expect("create out dir");
    let out_file = File::create(out_path.join(RESULTS_FILE)).expect("create results.txt");
    let mut writer = BufWriter::with_capacity(1 << 20, out_file);

    let t_eval = std::time::Instant::now();
    let mut buf_a: Vec<u32> = Vec::new();
    let mut buf_b: Vec<u32> = Vec::new();
    let mut pm: Vec<u32> = Vec::new();
    let mut order: Vec<u32> = Vec::new();
    let mut scratch: Vec<(usize, u32)> = Vec::new();
    let mut line: Vec<u8> = Vec::with_capacity(256);

    for q in &queries.items {
        let terms = &queries.terms[q.t_off as usize..(q.t_off + q.t_len) as usize];
        if q.phrase {
            run_phrase(&index, &doc_store, terms, &mut buf_a, &mut buf_b, &mut pm, &mut order);
            format_result(&mut writer, &mut line, &pm);
        } else {
            let ops = &queries.ops[q.o_off as usize..(q.o_off + q.t_len - 1) as usize];
            let r = run_bool(&index, terms, ops, &mut buf_a, &mut buf_b, &mut scratch);
            format_result(&mut writer, &mut line, r);
        }
    }
    writer.flush().expect("flush");
    let eval_dur = t_eval.elapsed();
    let c3 = cpu_ns();

    eprintln!("[PROFILE2] Queries file read: {:?} | Docs file read: {:?} | Distinct terms: {} | Queries: {}", t_qread, t_read, term_table.len(), queries.items.len());
    eprintln!("[PROFILE] Queries parse: {:?} | Corpus index: {:?} | Query eval+write: {:?}", t_queries, t_corpus, eval_dur);
    eprintln!("[PROFILE3] Peak RSS: {} MB", peak_rss_mb());
    eprintln!("[PROFILE7] CPU ms: queries {} | corpus {} | eval+write {} | total {}",
        (c1 - c0) / 1_000_000, (c2 - c1) / 1_000_000, (c3 - c2) / 1_000_000, (c3 - c0) / 1_000_000);
    eprintln!("[PROFILE6] CPU time user+sys: {:.2} s | VmPeak: {} MB", cpu_seconds(), vm_peak_mb());
}