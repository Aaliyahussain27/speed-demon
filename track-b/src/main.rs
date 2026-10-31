use std::collections::HashMap;
use std::env;
use std::hash::{BuildHasherDefault, Hasher};
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;

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

struct DocStore {
    raw: Vec<u8>,
    spans: Vec<Vec<(u32, u32)>>,
    starts: Vec<u32>,
    trans: [u8; 256],
}
impl DocStore {
    fn has_phrase(&self, id: u32, target: &[u8], words: &[(u32, u32)], toks: &mut Vec<(u32, u32)>) -> bool {
        let ci = self.starts.partition_point(|&s| s <= id) - 1;
        let (s, e) = self.spans[ci][(id - self.starts[ci]) as usize];
        let text = &self.raw[s as usize..e as usize];
        let k = words.len();
        toks.clear();
        let mut start = usize::MAX;
        for (i, &b) in text.iter().enumerate() {
            if self.trans[b as usize] != b' ' {
                if start == usize::MAX { start = i; }
            } else if start != usize::MAX {
                toks.push((start as u32, i as u32));
                start = usize::MAX;
            }
        }
        if start != usize::MAX { toks.push((start as u32, text.len() as u32)); }
        if toks.len() < k { return false; }
        'outer: for i in 0..=toks.len() - k {
            for j in 0..k {
                let (a, b) = toks[i + j];
                let (wa, wb) = words[j];
                let w = &target[wa as usize..wb as usize];
                if (b - a) as usize != w.len() { continue 'outer; }
                let t = &text[a as usize..b as usize];
                for (x, &y) in t.iter().zip(w.iter()) {
                    if self.trans[*x as usize] != y { continue 'outer; }
                }
            }
            return true;
        }
        false
    }
}

#[inline(always)]
fn record(map: &FastMap<Vec<u8>, u32>, postings: &mut [Vec<u32>], tok: &[u8], doc_id: u32) {
    if let Some(&tid) = map.get(tok) {
        let p = &mut postings[tid as usize];
        if p.last().copied() != Some(doc_id) { p.push(doc_id); }
    }
}

#[cfg(unix)]
fn read_docs(path: &Path) -> Vec<u8> {
    use std::os::unix::fs::FileExt;
    let f = File::open(path).expect("open docs.txt");
    let len = f.metadata().expect("stat docs.txt").len() as usize;
    let mut raw: Vec<u8> = vec![0u8; len];
    if len > 0 {
        let per = ((len + 3) / 4).max(1);
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

fn load_corpus(raw: Vec<u8>, term_ids: &FastMap<Vec<u8>, u32>) -> (DocStore, InvertedIndex) {

    let trans_table = build_trans_table();
    let nthreads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
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

    type ChunkResult = (Vec<(u32, u32)>, Vec<Vec<u32>>);

    let results: Vec<ChunkResult> = std::thread::scope(|s| {
        let raw_ref      = &raw[..];
        let term_ids_ref = term_ids;
        let mut handles = Vec::with_capacity(nthreads);

        for i in 0..nthreads {
            let bstart = byte_starts[i];
            let bend   = byte_starts[i + 1];

            handles.push(s.spawn(move || {
                let chunk = &raw_ref[bstart..bend];
                let mut local_postings: Vec<Vec<u32>> = vec![Vec::new(); n_terms];
                let mut spans: Vec<(u32, u32)> = Vec::with_capacity(chunk.len() / 40);
                let mut tok: Vec<u8> = Vec::with_capacity(64);
                let mut doc_id: u32 = 0;
                let mut pos = 0usize;

                for line in chunk.split(|&b| b == b'\n') {
                    let line_off = pos;
                    pos += line.len() + 1;
                    let line = if line.ends_with(b"\r") { &line[..line.len()-1] } else { line };
                    if line.is_empty() { continue; }

                    let ts = match line.iter().position(|&b| b == b'\t') {
                        Some(t) => t + 1,
                        None    => line.len(),
                    };
                    let text = &line[ts..];

                    for &b in text {
                        let norm = trans_table[b as usize];
                        if norm != b' ' {
                            tok.push(norm);
                        } else if !tok.is_empty() {
                            record(term_ids_ref, &mut local_postings, &tok, doc_id);
                            tok.clear();
                        }
                    }
                    if !tok.is_empty() {
                        record(term_ids_ref, &mut local_postings, &tok, doc_id);
                        tok.clear();
                    }

                    let st = (bstart + line_off + ts) as u32;
                    spans.push((st, st + text.len() as u32));
                    doc_id += 1;
                }
                (spans, local_postings)
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut doc_id_starts: Vec<u32> = Vec::with_capacity(nthreads + 1);
    let mut cumulative: u32 = 0;
    doc_id_starts.push(0);
    for (spans, _) in &results {
        cumulative += spans.len() as u32;
        doc_id_starts.push(cumulative);
    }

    let mut term_counts: Vec<u32> = vec![0; n_terms];
    for (_, pp) in &results {
        for (tid, p) in pp.iter().enumerate() { term_counts[tid] += p.len() as u32; }
    }
    let mut inv_offsets: Vec<u32> = Vec::with_capacity(n_terms + 1);
    let mut total: u32 = 0;
    inv_offsets.push(0);
    for &c in &term_counts { total += c; inv_offsets.push(total); }
    let mut doc_ids: Vec<u32> = vec![0u32; total as usize];
    let mut write_pos = inv_offsets[..n_terms].to_vec();

    let mut span_sets: Vec<Vec<(u32, u32)>> = Vec::with_capacity(nthreads);

    for (ci, (spans, pp)) in results.into_iter().enumerate() {
        let doc_off = doc_id_starts[ci];
        span_sets.push(spans);
        for (tid, postings) in pp.into_iter().enumerate() {
            let len = postings.len();
            if len > 0 {
                let wp = write_pos[tid] as usize;
                for (d, &p) in doc_ids[wp..wp+len].iter_mut().zip(postings.iter()) { *d = p + doc_off; }
                write_pos[tid] += len as u32;
            }
        }
    }

    (DocStore { raw, spans: span_sets, starts: doc_id_starts, trans: trans_table },
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

fn run_bool<'a>(
    index: &'a InvertedIndex, terms: &[u32], ops: &[Op],
    buf_a: &'a mut Vec<u32>, buf_b: &'a mut Vec<u32>,
) -> &'a [u32] {
    let first = index.get(terms[0]);
    if ops.is_empty() { return first; }
    buf_a.clear(); buf_a.extend_from_slice(first);
    let mut in_a = true;
    for (op, &tid) in ops.iter().zip(terms[1..].iter()) {
        let p = index.get(tid);
        match op {
            Op::And => { if in_a { if buf_a.is_empty() { continue; } intersect(buf_a, p, buf_b); in_a=false; }
                         else    { if buf_b.is_empty() { continue; } intersect(buf_b, p, buf_a); in_a=true;  } }
            Op::Or  => { if in_a { union(buf_a, p, buf_b); in_a=false; }
                         else    { union(buf_b, p, buf_a); in_a=true;  } }
            Op::Not => { if in_a { if buf_a.is_empty() { continue; } difference(buf_a, p, buf_b); in_a=false; }
                         else    { if buf_b.is_empty() { continue; } difference(buf_b, p, buf_a); in_a=true;  } }
        }
    }
    if in_a { buf_a } else { buf_b }
}

fn run_phrase(
    index: &InvertedIndex, doc_store: &DocStore, terms: &[u32], target: &[u8],
    buf_a: &mut Vec<u32>, buf_b: &mut Vec<u32>, matches: &mut Vec<u32>,
    toks: &mut Vec<(u32, u32)>, words: &mut Vec<(u32, u32)>,
) {
    matches.clear();
    let first = index.get(terms[0]);
    if first.is_empty() { return; }
    buf_a.clear(); buf_a.extend_from_slice(first);
    let mut in_a = true;
    for &tid in &terms[1..] {
        let p = index.get(tid);
        if in_a { if buf_a.is_empty() { return; } intersect(buf_a, p, buf_b); in_a=false; }
        else    { if buf_b.is_empty() { return; } intersect(buf_b, p, buf_a); in_a=true;  }
    }
    let candidates = if in_a { buf_a.as_slice() } else { buf_b.as_slice() };
    if candidates.is_empty() { return; }
    words.clear();
    let n = target.len();
    let mut i = 0;
    while i < n {
        if target[i] == b' ' { i += 1; continue; }
        let st = i;
        while i < n && target[i] != b' ' { i += 1; }
        words.push((st as u32, i as u32));
    }
    for &doc_id in candidates {
        if doc_store.has_phrase(doc_id, target, words, toks) { matches.push(doc_id); }
    }
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

    let t0 = std::time::Instant::now();
    let qraw = read_queries(&data_path.join(QUERIES_FILE));
    let t_qread = t0.elapsed();

    let docs_path = data_path.join(DOCS_FILE);
    let reader = std::thread::spawn(move || read_docs(&docs_path));

    let (mut queries, term_to_id) = extract_queries(&qraw);
    let t_queries = t0.elapsed();

    let t1 = std::time::Instant::now();
    let raw = reader.join().expect("docs reader");
    let t_wait = t1.elapsed();

    let t2 = std::time::Instant::now();
    let (doc_store, index) = load_corpus(raw, &term_to_id);
    let t_corpus = t2.elapsed();

    {
        let Queries { items, terms, .. } = &mut queries;
        for q in items.iter() {
            if q.phrase {
                let t = &mut terms[q.t_off as usize..(q.t_off + q.t_len) as usize];
                t.sort_unstable_by_key(|&x| index.get(x).len());
            }
        }
    }

    std::fs::create_dir_all(out_path).expect("create out dir");
    let out_file = File::create(out_path.join(RESULTS_FILE)).expect("create results.txt");
    let mut writer = BufWriter::new(out_file);

    let nthreads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
    let chunk_size = (queries.items.len() + nthreads - 1) / nthreads;
    let query_chunks: Vec<&[Query]> = queries.items.chunks(chunk_size).collect();
    let terms_ref: &[u32] = &queries.terms;
    let ops_ref: &[Op] = &queries.ops;
    let targets_ref: &[u8] = &queries.targets;

    let t_eval = std::time::Instant::now();
    let index_ref     = &index;
    let doc_store_ref = &doc_store;

    let rendered: Vec<Vec<u8>> = std::thread::scope(|s| {
        let handles: Vec<_> = query_chunks.into_iter().map(|chunk| {
            s.spawn(move || {
                let mut out = Vec::with_capacity(chunk.len() * 32);
                let mut buf_a: Vec<u32> = Vec::new();
                let mut buf_b: Vec<u32> = Vec::new();
                let mut pm:    Vec<u32> = Vec::new();
                let mut toks: Vec<(u32, u32)> = Vec::new();
                let mut words: Vec<(u32, u32)> = Vec::new();
                for q in chunk {
                    let terms = &terms_ref[q.t_off as usize..(q.t_off + q.t_len) as usize];
                    if q.phrase {
                        let target = &targets_ref[q.g_off as usize..(q.g_off + q.g_len) as usize];
                        run_phrase(index_ref, doc_store_ref, terms, target, &mut buf_a, &mut buf_b, &mut pm, &mut toks, &mut words);
                        format_result(&mut out, &pm);
                    } else {
                        let ops = &ops_ref[q.o_off as usize..(q.o_off + q.t_len - 1) as usize];
                        let r = run_bool(index_ref, terms, ops, &mut buf_a, &mut buf_b);
                        format_result(&mut out, r);
                    }
                }
                out
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let eval_dur = t_eval.elapsed();
    for buf in rendered { writer.write_all(&buf).expect("write"); }
    writer.flush().expect("flush");

    eprintln!("[PROFILE2] Queries file read: {:?} | Distinct terms: {} | Queries: {}", t_qread, term_to_id.len(), queries.items.len());
    eprintln!("[PROFILE] Queries parse: {:?} | Docs read wait: {:?} | Corpus index: {:?} | Parallel query eval: {:?}",
        t_queries, t_wait, t_corpus, eval_dur);
    eprintln!("[PROFILE3] Peak RSS: {} MB", peak_rss_mb());
}