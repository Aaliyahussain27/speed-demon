
# Track C: Performance Log

Every optimization must have a corresponding live-board run ID.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | Reference, unchanged | — | 14.44 | 367 | 0 |
| 1 | Initial Rust project scaffold | — | — | — | — |
| 2 |Hand-written parser, array region lookup, parallel chunks, interned names with per-country bitsets, LTO profile |run-5f342c0b5e1c |0.455199 | 413339648 |33.06836 |
| 3 | pushed perf file | run-1bf491613ec8 |0.444793 |413933568 |33.313208 |