# Track A: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 32.05 | 228 | 0 |
| 1 | simple correct Rust version (BTreeMap, String keys, Vec per box, sort at end) | run-306157ea1a17|1.946818 |325275648 |27.586485 |
| 2 | streamed input through a 1 MB buffer, byte parsing, numeric hour slots, endpoint ids in a flat table, u32 latencies. All four changes in one push, so the gain is not split by change. | run-5172091c330a |0.778623 | 47550464 |43.709129|
| 3 | Replace the standard HashMap hash for endpoint names with FNV-1a (no crates). Build settings: LTO, codegen-units = 1, panic = abort. |  | | | |