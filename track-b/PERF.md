# Track B: performance log

Every step from the reference to your final version, with measurements recorded during development. Local experiments are labelled separately from official live-board runs.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---:|---:|---:|
| 0 | The reference, unchanged (Python) | `spec-reference` | 56.35 | 2,200 | 0 |
| 1 | Query-targeted indexing (Python prototype) | `local-proto-1` (local) | 2.53 (sample) | ~350 | — |
| 2 | C-speed translation table + normalized phrase search (Python) | `local-proto-2` (local) | 2.14 (local) | ~650 | — |
| 3 | Migrated to Rust (zero-dependency, sorted `Vec<u32>` postings, LTO) | `local-docker-check` (local) | 2.08 (including container startup) | — | — |
| 4 | Parallel corpus indexing, contiguous document arena, reusable query buffers, two-pointer Boolean operations and rarest-term-first phrase matching | `run-735799f96a91` | 4.649 | 746 | 25.905 |

**Final board commit:** `73bf0ec8345081f2d0e99e798743f369df4f5ba7`

*Local measurements are development benchmarks, not official board results. The 30 MB previously listed for Step 3 was Docker image size, not peak memory.*