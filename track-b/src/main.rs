use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
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
    let file = File::open(path).expect("Failed to open queries.txt");
    let reader = BufReader::new(file);

    let mut queries = Vec::new();
    let mut needed_terms = HashSet::new();

    for line in reader.split(b'\n') {
        let mut line = line.expect("Failed to read line");
        if line.ends_with(b"\r") {
            line.pop();
        }
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
    let file = File::open(path).expect("Failed to open docs.txt");
    let reader = BufReader::with_capacity(1024 * 1024, file);

    let trans_table = build_trans_table();

    let mut index: HashMap<Vec<u8>, Vec<u32>> = HashMap::with_capacity(needed_terms.len());
    for term in needed_terms {
        index.insert(term.clone(), Vec::new());
    }

    let mut arena_data = Vec::with_capacity(32 * 1024 * 1024);
    let mut arena_offsets = Vec::with_capacity(250_000);

    let mut clean = Vec::new();
    let mut doc_id: u32 = 0;

    for line in reader.split(b'\n') {
        let mut line = line.expect("Failed to read line");
        if line.ends_with(b"\r") {
            line.pop();
        }
        if line.is_empty() && doc_id > 0 {
            // Trailing empty lines
            continue;
        }

        let tab_pos = line.iter().position(|&b| b == b'\t');
        let text = if let Some(t) = tab_pos {
            &line[t + 1..]
        } else {
            b""
        };

        clean.clear();
        clean.push(b' ');
        for &b in text {
            clean.push(trans_table[b as usize]);
        }
        clean.push(b' ');

        let start_offset = arena_data.len() as u32;
        arena_data.push(b' ');

        let mut first = true;
        for_each_token(&clean, |tok| {
            if let Some(postings) = index.get_mut(tok) {
                if postings.last().copied() != Some(doc_id) {
                    postings.push(doc_id);
                }
            }
            if !first {
                arena_data.push(b' ');
            }
            arena_data.extend_from_slice(tok);
            first = false;
        });

        arena_data.push(b' ');
        let end_offset = arena_data.len() as u32;
        arena_offsets.push((start_offset, end_offset));

        doc_id += 1;
    }

    (
        DocArena {
            data: arena_data,
            offsets: arena_offsets,
        },
        index,
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
