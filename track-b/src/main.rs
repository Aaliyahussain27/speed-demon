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

fn load_corpus(
    path: &Path,
    needed_terms: &HashSet<Vec<u8>>,
) -> (Vec<Vec<u8>>, HashMap<Vec<u8>, Vec<u32>>) {
    let file = File::open(path).expect("Failed to open docs.txt");
    let reader = BufReader::with_capacity(1024 * 1024, file);

    let trans_table = build_trans_table();

    let mut index: HashMap<Vec<u8>, Vec<u32>> = HashMap::with_capacity(needed_terms.len());
    for term in needed_terms {
        index.insert(term.clone(), Vec::new());
    }

    let mut norm_docs = Vec::new();
    let mut clean = Vec::new();
    let mut compact_doc = Vec::new();
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

        compact_doc.clear();
        compact_doc.push(b' ');

        let mut first = true;
        {
            let mut doc_seen: HashSet<&[u8]> = HashSet::new();
            for_each_token(&clean, |tok| {
                if doc_seen.insert(tok) {
                    if let Some(postings) = index.get_mut(tok) {
                        postings.push(doc_id);
                    }
                }
                if !first {
                    compact_doc.push(b' ');
                }
                compact_doc.extend_from_slice(tok);
                first = false;
            });
        }

        compact_doc.push(b' ');
        norm_docs.push(compact_doc.clone());

        doc_id += 1;
    }

    (norm_docs, index)
}

fn intersect(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut res = Vec::new();
    let mut i = 0;
    let mut j = 0;
    let na = a.len();
    let nb = b.len();
    while i < na && j < nb {
        let va = a[i];
        let vb = b[j];
        if va == vb {
            res.push(va);
            i += 1;
            j += 1;
        } else if va < vb {
            i += 1;
        } else {
            j += 1;
        }
    }
    res
}

fn union(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut res = Vec::with_capacity(a.len() + b.len());
    let mut i = 0;
    let mut j = 0;
    let na = a.len();
    let nb = b.len();
    while i < na && j < nb {
        let va = a[i];
        let vb = b[j];
        if va == vb {
            res.push(va);
            i += 1;
            j += 1;
        } else if va < vb {
            res.push(va);
            i += 1;
        } else {
            res.push(vb);
            j += 1;
        }
    }
    if i < na {
        res.extend_from_slice(&a[i..]);
    } else if j < nb {
        res.extend_from_slice(&b[j..]);
    }
    res
}

fn difference(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut res = Vec::new();
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
            res.push(va);
            i += 1;
        } else {
            j += 1;
        }
    }
    if i < na {
        res.extend_from_slice(&a[i..]);
    }
    res
}

fn run_bool(
    index: &HashMap<Vec<u8>, Vec<u32>>,
    terms: &[Vec<u8>],
    ops: &[Op],
) -> Vec<u32> {
    let empty = Vec::new();
    let mut running = index.get(&terms[0]).unwrap_or(&empty).clone();

    for (op, term) in ops.iter().zip(terms[1..].iter()) {
        let postings = index.get(term).unwrap_or(&empty);
        match op {
            Op::And => {
                if running.is_empty() {
                    continue;
                }
                running = intersect(&running, postings);
            }
            Op::Or => {
                running = union(&running, postings);
            }
            Op::Not => {
                if running.is_empty() {
                    continue;
                }
                running = difference(&running, postings);
            }
        }
    }
    running
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
    norm_docs: &[Vec<u8>],
    terms: &[Vec<u8>],
) -> Vec<u32> {
    let empty = Vec::new();

    // Start with the rarest term to minimize candidate list immediately
    let mut sorted_terms: Vec<&Vec<u8>> = terms.iter().collect();
    sorted_terms.sort_by_key(|t| index.get(*t).map_or(0, |p| p.len()));

    let mut candidates = index.get(sorted_terms[0]).unwrap_or(&empty).clone();
    if candidates.is_empty() {
        return Vec::new();
    }

    for term in &sorted_terms[1..] {
        let postings = index.get(*term).unwrap_or(&empty);
        candidates = intersect(&candidates, postings);
        if candidates.is_empty() {
            return Vec::new();
        }
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

    let mut matches = Vec::new();
    for &doc_id in &candidates {
        if contains_subslice(&norm_docs[doc_id as usize], &target) {
            matches.push(doc_id);
        }
    }
    matches
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

    let (queries, needed_terms) = extract_queries(&data_path.join(QUERIES_FILE));
    let (norm_docs, index) = load_corpus(&data_path.join(DOCS_FILE), &needed_terms);

    std::fs::create_dir_all(out_path).expect("Failed to create out directory");
    let out_file = File::create(out_path.join(RESULTS_FILE)).expect("Failed to create results.txt");
    let mut writer = BufWriter::new(out_file);

    for query in &queries {
        match query {
            Query::Bool(terms, ops) => {
                let res = run_bool(&index, terms, ops);
                format_result(&mut writer, &res);
            }
            Query::Phrase(terms) => {
                let res = run_phrase(&index, &norm_docs, terms);
                format_result(&mut writer, &res);
            }
        }
    }
}
