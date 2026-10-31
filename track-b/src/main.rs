use std::collections::{HashMap, HashSet};
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
    Bool(Vec<Vec<u8>>, Vec<Op>),
    Phrase(Vec<Vec<u8>>),
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

fn for_each_token<'a, F>(bytes: &'a [u8], mut callback: F)
where
    F: FnMut(&'a [u8]),
{
    let mut start = None;
    for (i, &b) in bytes.iter().enumerate() {
        if b != b' ' {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start {
            callback(&bytes[s..i]);
            start = None;
        }
    }
    if let Some(s) = start {
        callback(&bytes[s..]);
    }
}

fn extract_queries(path: &Path) -> (Vec<Query>, HashSet<Vec<u8>>) {
    let mut raw = Vec::new();
    File::open(path)
        .expect("Failed to open queries.txt")
        .read_to_end(&mut raw)
        .expect("Failed to read queries.txt");

    let mut queries = Vec::new();
    let mut needed_terms = HashSet::new();

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

                terms.push(parts[0].to_vec());
                needed_terms.insert(parts[0].to_vec());

                let mut i = 1;
                while i < parts.len() {
                    let op = match parts[i] {
                        b"AND" => Op::And,
                        b"OR" => Op::Or,
                        b"NOT" => Op::Not,
                        _ => panic!("Unknown operator {:?}", parts[i]),
                    };
                    ops.push(op);
                    let term = parts[i + 1].to_vec();
                    needed_terms.insert(term.clone());
                    terms.push(term);
                    i += 2;
                }
                queries.push(Query::Bool(terms, ops));
            } else if kind == b"PHRASE" {
                let inner = if rest.starts_with(b"\"") && rest.ends_with(b"\"") {
                    &rest[1..rest.len() - 1]
                } else {
                    rest
                };
                let terms: Vec<Vec<u8>> = inner
                    .split(|&b| b == b' ')
                    .map(|s| {
                        needed_terms.insert(s.to_vec());
                        s.to_vec()
                    })
                    .collect();
                queries.push(Query::Phrase(terms));
            }
        }
    }

    (queries, needed_terms)
}

struct DocArena {
    data: Vec<u8>,
    offsets: Vec<(u32, u32)>,
}

impl DocArena {
    #[inline(always)]
    fn get_doc(&self, id: u32) -> &[u8] {
        let (start, end) = self.offsets[id as usize];
        &self.data[start as usize..end as usize]
    }
}

fn load_corpus(
    path: &Path,
    needed_terms: &HashSet<Vec<u8>>,
) -> (DocArena, HashMap<Vec<u8>, Vec<u32>>) {
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

    // Assign each needed term a stable u32 ID — threads use a flat Vec<Vec<u32>>
    // indexed by term_id, avoiding per-thread HashMap construction + key cloning.
    let terms_ordered: Vec<&Vec<u8>> = needed_terms.iter().collect();
    let n_terms = terms_ordered.len();
    // term_ids: &[u8] → u32 (content-based lookup via Borrow<[u8]>)
    let term_ids: HashMap<&[u8], u32> = terms_ordered
        .iter()
        .enumerate()
        .map(|(i, t)| (t.as_slice(), i as u32))
        .collect();

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
        // Reborrow as shared references (&T is Copy) so each move|| closure
        // copies the fat pointer rather than trying to move the owned value.
        let raw_ref      = &raw[..];       // &[u8]                — Copy
        let term_ids_ref = &term_ids;      // &HashMap<&[u8], u32> — Copy
        let mut handles = Vec::with_capacity(nthreads);
        for i in 0..nthreads {
            // Pass byte indices (usize — Copy) and doc_id_start (u32 — Copy)
            let start        = byte_starts[i];
            let end          = byte_starts[i + 1];
            let doc_id_start = doc_id_starts[i];
            handles.push(s.spawn(move || {
                let chunk = &raw_ref[start..end];
                // Flat Vec indexed by term_id — zero string allocation in hot path
                let mut local_postings: Vec<Vec<u32>> = vec![Vec::new(); n_terms];
                let mut arena_data: Vec<u8>       = Vec::new();
                let mut arena_offsets: Vec<(u32, u32)> = Vec::new();
                let mut clean: Vec<u8>            = Vec::new();
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
                        None    => b"",
                    };

                    clean.clear();
                    clean.push(b' ');
                    for &b in text {
                        clean.push(trans_table[b as usize]);
                    }
                    clean.push(b' ');

                    let start_offset = arena_data.len() as u32;
                    arena_data.push(b' ');

                    for_each_token(&clean, |tok| {
                        // One lookup into the shared read-only term_ids map
                        if let Some(&tid) = term_ids_ref.get(tok) {
                            let postings = &mut local_postings[tid as usize];
                            if postings.last().copied() != Some(doc_id) {
                                postings.push(doc_id);
                            }
                        }
                        arena_data.extend_from_slice(tok);
                        arena_data.push(b' ');
                    });

                    let end_offset = arena_data.len() as u32;
                    arena_offsets.push((start_offset, end_offset));
                    doc_id += 1;
                }

                (arena_data, arena_offsets, local_postings)
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // --- Merge phase ---
    // Concat arenas (adjust offsets); concat posting lists per term_id
    // (already sorted ascending since threads own disjoint doc_id ranges).
    let mut merged_data: Vec<u8>           = Vec::with_capacity(32 * 1024 * 1024);
    let mut merged_offsets: Vec<(u32, u32)> = Vec::with_capacity(250_000);
    let mut final_postings: Vec<Vec<u32>>  = vec![Vec::new(); n_terms];

    for (partial_data, partial_offsets, partial_postings) in results {
        let base = merged_data.len() as u32;
        merged_data.extend_from_slice(&partial_data);
        for (s, e) in partial_offsets {
            merged_offsets.push((s + base, e + base));
        }
        for (tid, postings) in partial_postings.into_iter().enumerate() {
            if !postings.is_empty() {
                final_postings[tid].extend_from_slice(&postings);
            }
        }
    }

    // Convert flat Vec back to the HashMap the rest of the code expects
    let mut final_index: HashMap<Vec<u8>, Vec<u32>> = HashMap::with_capacity(n_terms);
    for (tid, term) in terms_ordered.iter().enumerate() {
        final_index.insert(
            (*term).clone(),
            std::mem::take(&mut final_postings[tid]),
        );
    }

    (
        DocArena {
            data: merged_data,
            offsets: merged_offsets,
        },
        final_index,
    )
}



fn intersect(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
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
            i += 1;
        } else {
            j += 1;
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

fn difference(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let mut i = 0;
    let mut j = 0;
    let na = a.len();
    let nb = b.len();
    while i < na && j < nb {
        let va = a[i];
        let vb = b[j];
        if va == vb {
            i += 1;
            j += 1;
        } else if va < vb {
            out.push(va);
            i += 1;
        } else {
            j += 1;
        }
    }
    if i < na {
        out.extend_from_slice(&a[i..]);
    }
}

fn run_bool<'a>(
    index: &'a HashMap<Vec<u8>, Vec<u32>>,
    terms: &[Vec<u8>],
    ops: &[Op],
    buf_a: &'a mut Vec<u32>,
    buf_b: &'a mut Vec<u32>,
) -> &'a [u32] {
    let empty = &[];
    let first = index.get(&terms[0]).map(|v| v.as_slice()).unwrap_or(empty);

    if ops.is_empty() {
        return first;
    }

    buf_a.clear();
    buf_a.extend_from_slice(first);

    let mut current_in_a = true;

    for (op, term) in ops.iter().zip(terms[1..].iter()) {
        let postings = index.get(term).map(|v| v.as_slice()).unwrap_or(empty);
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
    index: &HashMap<Vec<u8>, Vec<u32>>,
    doc_arena: &DocArena,
    terms: &[Vec<u8>],
    buf_a: &mut Vec<u32>,
    buf_b: &mut Vec<u32>,
    matches: &mut Vec<u32>,
) {
    matches.clear();
    let empty = &[];

    // Start with the rarest term to minimize candidate list immediately
    let mut sorted_terms: Vec<&Vec<u8>> = terms.iter().collect();
    sorted_terms.sort_by_key(|t| index.get(*t).map_or(0, |p| p.len()));

    let first = index.get(sorted_terms[0]).map(|v| v.as_slice()).unwrap_or(empty);
    if first.is_empty() {
        return;
    }

    buf_a.clear();
    buf_a.extend_from_slice(first);
    let mut current_in_a = true;

    for term in &sorted_terms[1..] {
        let postings = index.get(*term).map(|v| v.as_slice()).unwrap_or(empty);
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

    let mut target = Vec::new();
    target.push(b' ');
    for (i, t) in terms.iter().enumerate() {
        if i > 0 {
            target.push(b' ');
        }
        target.extend_from_slice(t);
    }
    target.push(b' ');

    for &doc_id in candidates {
        if contains_subslice(doc_arena.get_doc(doc_id), &target) {
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
    let (queries, needed_terms) = extract_queries(&data_path.join(QUERIES_FILE));
    let t_queries = t0.elapsed();

    let t1 = std::time::Instant::now();
    let (doc_arena, index) = load_corpus(&data_path.join(DOCS_FILE), &needed_terms);
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
            Query::Bool(terms, ops) => {
                let q_start = std::time::Instant::now();
                let res = run_bool(&index, terms, ops, &mut buf_a, &mut buf_b);
                bool_dur += q_start.elapsed();
                format_result(&mut writer, res);
            }
            Query::Phrase(terms) => {
                let q_start = std::time::Instant::now();
                run_phrase(&index, &doc_arena, terms, &mut buf_a, &mut buf_b, &mut phrase_matches);
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
