use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;

const DOCS_FILE: &str = "docs.txt";
const QUERIES_FILE: &str = "queries.txt";
const RESULTS_FILE: &str = "results.txt";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    And,
    Or,
    Not,
}

enum Query {
    Bool {
        terms: Vec<u32>,
        ops: Vec<Op>,
    },
    Phrase {
        terms: Vec<u32>,
        target: Vec<u8>,
    },
}

fn build_trans_table() -> [u8; 256] {
    let mut table = [b' '; 256];
    for b in b'0'..=b'9' {
        table[b as usize] = b;
    }
    for b in b'a'..=b'z' {
        table[b as usize] = b;
    }
    for b in b'A'..=b'Z' {
        table[b as usize] = b + 32;
    }
    table
}

fn extract_queries(path: &Path) -> (Vec<Query>, HashMap<Vec<u8>, u32>) {
    let mut raw = Vec::new();
    File::open(path)
        .expect("Failed to open queries.txt")
        .read_to_end(&mut raw)
        .expect("Failed to read queries.txt");

    let mut queries = Vec::new();
    let mut term_to_id: HashMap<Vec<u8>, u32> = HashMap::new();

    for line in raw.split(|&b| b == b'\n') {
        let line = if line.ends_with(b"\r") {
            &line[..line.len() - 1]
        } else {
            line
        };
        if line.is_empty() {
            continue;
        }

        let first_space = line.iter().position(|&b| b == b' ');
        if let Some(pos) = first_space {
            let kind = &line[..pos];
            let rest = &line[pos + 1..];

            if kind == b"BOOL" {
                let parts: Vec<&[u8]> = rest.split(|&b| b == b' ').collect();
                let mut terms = Vec::with_capacity((parts.len() + 1) / 2);
                let mut ops = Vec::with_capacity(parts.len() / 2);

                let next_id = term_to_id.len() as u32;
                let id0 = *term_to_id.entry(parts[0].to_vec()).or_insert(next_id);
                terms.push(id0);

                let mut i = 1;
                while i < parts.len() {
                    let op = match parts[i] {
                        b"AND" => Op::And,
                        b"OR" => Op::Or,
                        b"NOT" => Op::Not,
                        _ => panic!("Unknown operator {:?}", parts[i]),
                    };
                    ops.push(op);
                    let next_id = term_to_id.len() as u32;
                    let id = *term_to_id.entry(parts[i + 1].to_vec()).or_insert(next_id);
                    terms.push(id);
                    i += 2;
                }
                queries.push(Query::Bool { terms, ops });
            } else if kind == b"PHRASE" {
                let inner = if rest.starts_with(b"\"") && rest.ends_with(b"\"") {
                    &rest[1..rest.len() - 1]
                } else {
                    rest
                };
                let words: Vec<&[u8]> = inner.split(|&b| b == b' ').collect();
                let mut term_ids = Vec::with_capacity(words.len());
                let mut target = Vec::new();
                target.push(b' ');
                for (idx, w) in words.iter().enumerate() {
                    if idx > 0 {
                        target.push(b' ');
                    }
                    target.extend_from_slice(w);
                    let next_id = term_to_id.len() as u32;
                    let id = *term_to_id.entry(w.to_vec()).or_insert(next_id);
                    term_ids.push(id);
                }
                target.push(b' ');
                queries.push(Query::Phrase { terms: term_ids, target });
            }
        }
    }

    (queries, term_to_id)
}

struct DocArena {
    chunks: Vec<Vec<u8>>,
    offsets: Vec<(u8, u32, u32)>,
}

impl DocArena {
    #[inline(always)]
    fn get_doc(&self, id: u32) -> &[u8] {
        let (chunk_idx, start, end) = self.offsets[id as usize];
        &self.chunks[chunk_idx as usize][start as usize..end as usize]
    }
}

fn load_corpus(
    path: &Path,
    term_ids: &HashMap<Vec<u8>, u32>,
) -> (DocArena, Vec<Vec<u32>>) {
    let mut raw: Vec<u8> = Vec::with_capacity(16 * 1024 * 1024);
    File::open(path)
        .expect("Failed to open docs.txt")
        .read_to_end(&mut raw)
        .expect("Failed to read docs.txt");

    let trans_table = build_trans_table();

    let nthreads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8);

    let n_terms = term_ids.len();

    // Find chunk boundaries at line starts so no line is split across threads
    let mut byte_starts: Vec<usize> = vec![0];
    for i in 1..nthreads {
        let mut pos = (i * raw.len()) / nthreads;
        while pos < raw.len() && raw[pos] != b'\n' {
            pos += 1;
        }
        if pos < raw.len() {
            pos += 1; // skip the newline itself
        }
        byte_starts.push(pos);
    }
    byte_starts.push(raw.len());

    // Pre-count non-empty docs per chunk so each thread knows its doc_id range
    let mut doc_id_starts: Vec<u32> = vec![0];
    let mut cumulative: u32 = 0;
    for i in 0..nthreads {
        let chunk = &raw[byte_starts[i]..byte_starts[i + 1]];
        let count = chunk
            .split(|&b| b == b'\n')
            .filter(|line| {
                let line = if line.ends_with(b"\r") { &line[..line.len() - 1] } else { line };
                !line.is_empty()
            })
            .count() as u32;
        cumulative += count;
        doc_id_starts.push(cumulative);
    }

    // Each thread returns: (arena_data, arena_offsets, Vec<Vec<u32>> by term_id)
    type ChunkResult = (Vec<u8>, Vec<(u32, u32)>, Vec<Vec<u32>>);

    let results: Vec<ChunkResult> = std::thread::scope(|s| {
        let raw_ref = &raw[..];
        let term_ids_ref = term_ids;
        let mut handles = Vec::with_capacity(nthreads);
        for i in 0..nthreads {
            let start = byte_starts[i];
            let end = byte_starts[i + 1];
            let doc_id_start = doc_id_starts[i];
            handles.push(s.spawn(move || {
                let chunk = &raw_ref[start..end];
                let mut local_postings: Vec<Vec<u32>> = vec![Vec::new(); n_terms];
                let mut arena_data: Vec<u8> = Vec::new();
                let mut arena_offsets: Vec<(u32, u32)> = Vec::new();
                let mut doc_id = doc_id_start;

                for line in chunk.split(|&b| b == b'\n') {
                    let line = if line.ends_with(b"\r") {
                        &line[..line.len() - 1]
                    } else {
                        line
                    };
                    if line.is_empty() { continue; }

                    let text = match line.iter().position(|&b| b == b'\t') {
                        Some(t) => &line[t + 1..],
                        None => b"",
                    };

                    let start_offset = arena_data.len() as u32;
                    arena_data.push(b' ');

                    let mut in_token = false;
                    let mut tok_start = 0;

                    for &b in text {
                        let norm = trans_table[b as usize];
                        if norm != b' ' {
                            if !in_token {
                                in_token = true;
                                tok_start = arena_data.len();
                            }
                            arena_data.push(norm);
                        } else if in_token {
                            let tok = &arena_data[tok_start..];
                            if let Some(&tid) = term_ids_ref.get(tok) {
                                let postings = &mut local_postings[tid as usize];
                                if postings.last().copied() != Some(doc_id) {
                                    postings.push(doc_id);
                                }
                            }
                            arena_data.push(b' ');
                            in_token = false;
                        }
                    }

                    if in_token {
                        let tok = &arena_data[tok_start..];
                        if let Some(&tid) = term_ids_ref.get(tok) {
                            let postings = &mut local_postings[tid as usize];
                            if postings.last().copied() != Some(doc_id) {
                                postings.push(doc_id);
                            }
                        }
                        arena_data.push(b' ');
                    } else if arena_data.len() as u32 == start_offset + 1 {
                        arena_data.push(b' ');
                    }

                    let end_offset = arena_data.len() as u32;
                    arena_offsets.push((start_offset, end_offset));
                    doc_id += 1;
                }

                (arena_data, arena_offsets, local_postings)
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Drop the large raw file buffer immediately to release memory before merge
    drop(raw);

    // --- Merge phase ---
    let mut chunks: Vec<Vec<u8>> = Vec::with_capacity(results.len());
    let mut offsets: Vec<(u8, u32, u32)> = Vec::with_capacity(250_000);
    let mut final_postings: Vec<Vec<u32>> = vec![Vec::new(); n_terms];

    for (chunk_idx, (partial_data, partial_offsets, partial_postings)) in results.into_iter().enumerate() {
        let c_idx = chunk_idx as u8;
        chunks.push(partial_data);
        for (s, e) in partial_offsets {
            offsets.push((c_idx, s, e));
        }
        for (tid, postings) in partial_postings.into_iter().enumerate() {
            if !postings.is_empty() {
                final_postings[tid].extend_from_slice(&postings);
            }
        }
    }

    (
        DocArena {
            chunks,
            offsets,
        },
        final_postings,
    )
}

#[inline(always)]
fn gallop(slice: &[u32], target: u32) -> usize {
    let n = slice.len();
    if n == 0 || slice[0] >= target {
        return 0;
    }
    let mut step = 1;
    let mut prev = 0;
    while step < n && slice[step] < target {
        prev = step;
        step <<= 1;
    }
    let high = step.min(n);
    let mut low = prev;
    let mut len = high - low;
    while len > 0 {
        let half = len / 2;
        let mid = low + half;
        if slice[mid] < target {
            low = mid + 1;
            len -= half + 1;
        } else {
            len = half;
        }
    }
    low
}

fn intersect(mut a: &[u32], mut b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    out.reserve(a.len().min(b.len()));

    while !a.is_empty() && !b.is_empty() {
        let va = a[0];
        let vb = b[0];
        if va == vb {
            out.push(va);
            a = &a[1..];
            b = &b[1..];
        } else if va < vb {
            if a.len() > 16 * b.len() {
                let idx = gallop(a, vb);
                a = &a[idx..];
            } else {
                a = &a[1..];
            }
        } else {
            if b.len() > 16 * a.len() {
                let idx = gallop(b, va);
                b = &b[idx..];
            } else {
                b = &b[1..];
            }
        }
    }
}

fn union(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    out.reserve(a.len() + b.len());
    let mut i = 0;
    let mut j = 0;
    let na = a.len();
    let nb = b.len();
    while i < na && j < nb {
        let va = a[i];
        let vb = b[j];
        if va == vb {
            out.push(va);
            i += 1;
            j += 1;
        } else if va < vb {
            out.push(va);
            i += 1;
        } else {
            out.push(vb);
            j += 1;
        }
    }
    if i < na {
        out.extend_from_slice(&a[i..]);
    } else if j < nb {
        out.extend_from_slice(&b[j..]);
    }
}

fn difference(mut a: &[u32], mut b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    out.reserve(a.len());

    while !a.is_empty() && !b.is_empty() {
        let va = a[0];
        let vb = b[0];
        if va == vb {
            a = &a[1..];
            b = &b[1..];
        } else if va < vb {
            out.push(va);
            a = &a[1..];
        } else {
            if b.len() > 16 * a.len() {
                let idx = gallop(b, va);
                b = &b[idx..];
            } else {
                b = &b[1..];
            }
        }
    }
    if !a.is_empty() {
        out.extend_from_slice(a);
    }
}

fn run_bool<'a>(
    index: &'a [Vec<u32>],
    terms: &[u32],
    ops: &[Op],
    buf_a: &'a mut Vec<u32>,
    buf_b: &'a mut Vec<u32>,
) -> &'a [u32] {
    let first = &index[terms[0] as usize];

    if ops.is_empty() {
        return first;
    }

    buf_a.clear();
    buf_a.extend_from_slice(first);

    let mut current_in_a = true;

    for (op, &term_id) in ops.iter().zip(terms[1..].iter()) {
        let postings = &index[term_id as usize];
        match op {
            Op::And => {
                if current_in_a {
                    if buf_a.is_empty() {
                        continue;
                    }
                    intersect(buf_a, postings, buf_b);
                    current_in_a = false;
                } else {
                    if buf_b.is_empty() {
                        continue;
                    }
                    intersect(buf_b, postings, buf_a);
                    current_in_a = true;
                }
            }
            Op::Or => {
                if current_in_a {
                    union(buf_a, postings, buf_b);
                    current_in_a = false;
                } else {
                    union(buf_b, postings, buf_a);
                    current_in_a = true;
                }
            }
            Op::Not => {
                if current_in_a {
                    if buf_a.is_empty() {
                        continue;
                    }
                    difference(buf_a, postings, buf_b);
                    current_in_a = false;
                } else {
                    if buf_b.is_empty() {
                        continue;
                    }
                    difference(buf_b, postings, buf_a);
                    current_in_a = true;
                }
            }
        }
    }

    if current_in_a {
        buf_a.as_slice()
    } else {
        buf_b.as_slice()
    }
}

// Fast substring search on byte slice
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    let n = needle.len();
    let h = haystack.len();
    if n == 0 {
        return true;
    }
    if h < n {
        return false;
    }
    let first = needle[0];
    let mut i = 0;
    while i + n <= h {
        if haystack[i] == first && &haystack[i..i + n] == needle {
            return true;
        }
        i += 1;
    }
    false
}

fn run_phrase(
    index: &[Vec<u32>],
    doc_arena: &DocArena,
    terms: &[u32],
    target: &[u8],
    buf_a: &mut Vec<u32>,
    buf_b: &mut Vec<u32>,
    matches: &mut Vec<u32>,
) {
    matches.clear();

    // Start with the rarest term to minimize candidate list immediately
    let mut sorted_terms: Vec<u32> = terms.to_vec();
    sorted_terms.sort_by_key(|&t| index[t as usize].len());
    let slice = &sorted_terms;

    let first = &index[slice[0] as usize];
    if first.is_empty() {
        return;
    }

    buf_a.clear();
    buf_a.extend_from_slice(first);
    let mut current_in_a = true;

    for &term_id in &slice[1..] {
        let postings = &index[term_id as usize];
        if current_in_a {
            if buf_a.is_empty() {
                return;
            }
            intersect(buf_a, postings, buf_b);
            current_in_a = false;
        } else {
            if buf_b.is_empty() {
                return;
            }
            intersect(buf_b, postings, buf_a);
            current_in_a = true;
        }
    }

    let candidates = if current_in_a {
        buf_a.as_slice()
    } else {
        buf_b.as_slice()
    };

    if candidates.is_empty() {
        return;
    }

    for &doc_id in candidates {
        if contains_subslice(doc_arena.get_doc(doc_id), target) {
            matches.push(doc_id);
        }
    }
}

fn format_result<W: Write>(writer: &mut W, ids: &[u32]) {
    if ids.is_empty() {
        writeln!(writer, "0 0").unwrap();
        return;
    }
    let count = ids.len();
    let sum: u64 = ids.iter().map(|&x| x as u64).sum();
    write!(writer, "{} {}", count, sum).unwrap();

    let take = count.min(10);
    for &id in &ids[..take] {
        write!(writer, " {}", id).unwrap();
    }
    writeln!(writer).unwrap();
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut data_dir = "/data".to_string();
    let mut out_dir = "/out".to_string();

    let mut i = 1;
    while i < args.len() {
        if args[i] == "--data" && i + 1 < args.len() {
            data_dir = args[i + 1].clone();
            i += 2;
        } else if args[i] == "--out" && i + 1 < args.len() {
            out_dir = args[i + 1].clone();
            i += 2;
        } else {
            i += 1;
        }
    }

    let data_path = Path::new(&data_dir);
    let out_path = Path::new(&out_dir);

    let t0 = std::time::Instant::now();
    let (queries, term_to_id) = extract_queries(&data_path.join(QUERIES_FILE));
    let t_queries = t0.elapsed();

    let t1 = std::time::Instant::now();
    let (doc_arena, index) = load_corpus(&data_path.join(DOCS_FILE), &term_to_id);
    let t_corpus = t1.elapsed();

    std::fs::create_dir_all(out_path).expect("Failed to create out directory");
    let out_file = File::create(out_path.join(RESULTS_FILE)).expect("Failed to create results.txt");
    let mut writer = BufWriter::new(out_file);

    let mut bool_dur = std::time::Duration::ZERO;
    let mut phrase_dur = std::time::Duration::ZERO;

    let mut buf_a: Vec<u32> = Vec::new();
    let mut buf_b: Vec<u32> = Vec::new();
    let mut phrase_matches: Vec<u32> = Vec::new();

    for query in &queries {
        match query {
            Query::Bool { terms, ops } => {
                let q_start = std::time::Instant::now();
                let res = run_bool(&index, terms, ops, &mut buf_a, &mut buf_b);
                bool_dur += q_start.elapsed();
                format_result(&mut writer, res);
            }
            Query::Phrase { terms, target } => {
                let q_start = std::time::Instant::now();
                run_phrase(&index, &doc_arena, terms, target, &mut buf_a, &mut buf_b, &mut phrase_matches);
                phrase_dur += q_start.elapsed();
                format_result(&mut writer, &phrase_matches);
            }
        }
    }

    eprintln!(
        "[PROFILE] Queries parse: {:?} | Corpus index: {:?} | BOOL eval: {:?} | PHRASE eval: {:?}",
        t_queries, t_corpus, bool_dur, phrase_dur
    );
}
