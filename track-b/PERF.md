# Track B: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged (Python) | spec-reference | 56.35 | 2,200 | 0 |
| 1 | Query-targeted indexing (Python) | local-proto-1 | 2.53 (sample) | ~350 | - |
| 2 | C-speed translation table + normalized phrase search (Python) | local-proto-2 | 2.14 (official) | ~650 | - |
| 3 | Migrated to Rust (zero-dependency, sorted Vec<u32> postings, LTO) | local-rust-final | < 0.8 | < 180 | - |
