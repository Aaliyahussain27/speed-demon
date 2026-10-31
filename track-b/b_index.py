#!/usr/bin/env python3.13
"""Optimized solution for Speed Demon 2026, Track B (Index).
- Bulk translation table tokenization (C-speed bytes.translate)
- Query-targeted inverted index (only query terms are indexed)
- Normalized text document cache with single spaces
- Substring search for phrase queries (avoids re-tokenizing candidate docs)
- Early-exit candidate filtering
"""
from __future__ import annotations

import argparse
import os
import sys

DOCS_FILE = "docs.txt"
QUERIES_FILE = "queries.txt"
RESULTS_FILE = "results.txt"

# Translation table: ASCII [A-Z] -> [a-z], [a-z0-9] kept, \t and \n kept, all others -> space (0x20)
TRANS_TABLE = bytearray(256)
for i in range(256):
    if (ord("a") <= i <= ord("z")) or (ord("0") <= i <= ord("9")):
        TRANS_TABLE[i] = i
    elif ord("A") <= i <= ord("Z"):
        TRANS_TABLE[i] = i + 32
    elif i == ord("\t") or i == ord("\n"):
        TRANS_TABLE[i] = i
    else:
        TRANS_TABLE[i] = ord(" ")
TRANS_TABLE = bytes(TRANS_TABLE)

EMPTY_SET: frozenset[int] = frozenset()


def extract_queries(queries_path: str) -> tuple[list[tuple[bytes, list[bytes]]], set[bytes]]:
    queries: list[tuple[bytes, list[bytes]]] = []
    needed_terms: set[bytes] = set()
    with open(queries_path, "rb") as f:
        for line in f:
            if line.endswith(b"\n"):
                line = line[:-1]
            kind, _, rest = line.partition(b" ")
            if kind == b"BOOL":
                words = rest.split(b" ")
                needed_terms.add(words[0])
                for term in words[2::2]:
                    needed_terms.add(term)
                queries.append((b"BOOL", words))
            elif kind == b"PHRASE":
                terms = rest[1:-1].split(b" ")
                for term in terms:
                    needed_terms.add(term)
                queries.append((b"PHRASE", terms))
            else:
                raise ValueError(f"bad query {line!r}")
    return queries, needed_terms


def load_corpus(path: str, needed_terms: set[bytes]) -> tuple[list[bytes], dict[bytes, set[int]]]:
    """Read docs.txt. Return normalized texts and index of needed terms."""
    with open(path, "rb") as f:
        content = f.read()

    # Bulk byte-translate
    translated = content.translate(TRANS_TABLE)
    lines = translated.splitlines()

    index: dict[bytes, set[int]] = {t: set() for t in needed_terms}
    norm_docs: list[bytes] = []

    for doc_id, line in enumerate(lines):
        tab_pos = line.find(b"\t")
        clean_text = line[tab_pos + 1:]
        words = clean_text.split()
        for w in set(words) & needed_terms:
            index[w].add(doc_id)
        # Store space-padded clean text for fast substring matching
        norm_docs.append(b" " + b" ".join(words) + b" ")

    return norm_docs, index


def run_bool(index: dict[bytes, set[int]], words: list[bytes]):
    result = index.get(words[0], EMPTY_SET)
    for op, term in zip(words[1::2], words[2::2]):
        postings = index.get(term, EMPTY_SET)
        if op == b"AND":
            result = result & postings
        elif op == b"OR":
            result = result | postings
        elif op == b"NOT":
            result = result - postings
        else:
            raise ValueError(f"unknown operator {op!r}")
    return result


def run_phrase(index: dict[bytes, set[int]], norm_docs: list[bytes], terms: list[bytes]) -> list[int]:
    # Intersect term sets to find candidates
    # Smallest candidate set first
    candidates = index.get(terms[0], EMPTY_SET)
    for term in terms[1:]:
        candidates = candidates & index.get(term, EMPTY_SET)
        if not candidates:
            return []

    target = b" " + b" ".join(terms) + b" "
    matches = []
    # If candidates set is small, test membership directly
    for doc_id in sorted(candidates):
        if target in norm_docs[doc_id]:
            matches.append(doc_id)
    return matches


def format_result(doc_ids) -> str:
    # If doc_ids is already a sorted list
    if isinstance(doc_ids, list):
        ids = doc_ids
    else:
        ids = sorted(doc_ids)
    if not ids:
        return "0 0"
    return " ".join(str(x) for x in [len(ids), sum(ids)] + ids[:10])


def main(data_dir: str, out_dir: str) -> None:
    queries_path = os.path.join(data_dir, QUERIES_FILE)
    queries, needed_terms = extract_queries(queries_path)

    norm_docs, index = load_corpus(os.path.join(data_dir, DOCS_FILE), needed_terms)

    results = []
    for kind, payload in queries:
        if kind == b"BOOL":
            results.append(format_result(run_bool(index, payload)))
        else:
            results.append(format_result(run_phrase(index, norm_docs, payload)))

    os.makedirs(out_dir, exist_ok=True)
    with open(os.path.join(out_dir, RESULTS_FILE), "w", encoding="ascii", newline="\n") as f:
        f.write("".join(r + "\n" for r in results))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Track B (Index) optimized solution.")
    parser.add_argument("--data", default="/data")
    parser.add_argument("--out", default="/out")
    args = parser.parse_args()
    main(args.data, args.out)
    sys.exit(0)
