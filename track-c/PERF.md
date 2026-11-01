
# Track C: Performance Log

Every optimization must have a corresponding live-board run ID.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | Reference, unchanged | — | 14.44 | 384827392 | 0 |
| 1 | Initial Rust project scaffold | — | — | — | — |
| 2 | Hand-written parser, array region lookup, parallel chunks, interned names with per-country bitsets, LTO profile | run-5f342c0b5e1c | 0.455199 | 413339648 | 33.06836 |
| 3 | Pushed performance-file update | run-1bf491613ec8 | 0.444793 | 413933568 | 33.313208 |
| 4 | Ownership-based global name merge and removal of duplicate worker name storage | Pending | 0.456 | 356000000 | 33.758 |
| 5 | Custom zero-dependency FxHasher for name interning and global lookup | Pending | 0.417 | 355000000 | 34.343 |
| 6 | Arena-based name storage using NameSpan, worker-local arenas and custom NameTable | Pending | — | — | 34.813 |
| 7 | Sparse distinct-name tracking using Vec<u32>, worker-local sort/dedup and sparse global merging | Pending | — | — | 36.530 |