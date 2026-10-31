# Track B: Index

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/b-index.md).

## Approach

Our solution replaces the naive inverted index and unindexed candidate re-tokenization with an optimized, memory-efficient search architecture:

1. **Pre-Query Analysis & Query-Targeted Indexing**:
   Because `queries.txt` is available alongside `docs.txt`, we parse all 50,000 queries first and harvest the vocabulary of needed terms (11,430 unique query terms in the sample dataset out of 378,185 corpus terms). Documents are indexed *only* for terms that appear in at least one query, discarding 97% of corpus terms and slashing memory allocations and dictionary lookups.

2. **C-Speed Byte Translation**:
   Instead of compiling and executing Python regular expressions (`TOKEN_RE.findall(text.lower())`) on each document line, we construct a 256-byte ASCII mapping table that folds `A-Z` to `a-z`, preserves `[a-z0-9]`, `\t`, and `\n`, and converts every other byte into a space `0x20`. Translating the raw bytes directly runs at C speed via `bytes.translate`.

3. **Normalized Substring Search for Phrase Queries**:
   The reference implementation re-tokenized raw document text from scratch for every candidate document across all phrase queries. We cache normalized, single-space document representations (`b' ' + b' '.join(words) + b' '`). This guarantees that matching a phrase query `"t1 t2 ... tk"` is equivalent to searching for `b' ' + b' '.join(terms) + b' '` using Python's highly optimized Boyer-Moore-Horspool C-level `bytes.__contains__` search.

4. **Early-Exit Boolean & Phrase Candidate Pruning**:
   In both Boolean chains and Phrase queries, intermediate set operations check for emptiness early (`if not candidates: return []`), terminating evaluation immediately when no matches remain.

## Dependencies

None. Python standard library only (`sys`, `os`, `argparse`).

## With another 24 hours

1. Multi-threaded / multiprocessing document ingestion across multiple CPU cores.
2. Roaring bitmaps or SIMD bitset vectorization for posting list intersections.
3. Positional inverted index with delta-encoded integer arrays.
