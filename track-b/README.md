# Track B: Index

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/b-index.md).

## Approach

Our solution is implemented in **Rust (2021 edition)**, reproducing and amplifying the optimizations established in the Python prototype:

1. **Query-Targeted Posting Indexing**:
   Because `queries.txt` is available alongside `docs.txt`, we parse all queries first and extract only the vocabulary of requested terms. The inverted index stores postings *strictly* for terms appearing in at least one query, discarding over 97% of corpus terms and radically reducing memory and cache pressure.

2. **Zero-Overhead ASCII Mapping & Tokenization**:
   Instead of regular expressions or Unicode case-folding, we construct a 256-element byte mapping table that folds ASCII uppercase `[A-Z]` to `[a-z]`, keeps `[a-z0-9]`, and treats all other bytes (including UTF-8 continuation bytes $\ge 0x80$) as separators (0x20).

3. **Sorted Vectors for Posting Lists**:
   Documents are processed in sequential order `0..N-1`. Postings are stored directly as contiguous, sorted `Vec<u32>` slices. This eliminates hash set overhead and allows Boolean `AND`, `OR`, and `NOT` operations to run via $O(N+M)$ cache-friendly two-pointer merge scans with early termination.

4. **Normalized Space-Padded Phrase Search**:
   Phrase queries search against pre-normalized, space-delimited byte representations of candidate documents using slice window matching.

## Dependencies

Zero external crates (`dependency_count = 0`). Standard library only (`std::collections`, `std::fs`, `std::io`, `std::env`).

## Build & Run

```bash
cargo build --release
./target/release/solution --data /data --out /out
```
