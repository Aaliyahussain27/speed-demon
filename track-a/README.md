# Track A: Ingest

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

Stage 1: 
Read the whole log into memory.
For each line, split it on spaces and keep the timestamp, endpoint, status and latency.
The hour is the first 13 characters of the timestamp, plus :00:00Z.
Store each (hour, endpoint) box in a BTreeMap. Each box keeps an error count and a Vec of latencies.
Sort each latency list. Read p50, p95 and p99 with the nearest-rank formula (p * n + 99) / 100.
Error rate is errors * 1000000 / count, in integers.
Write one line per box, in key order.

## Dependencies

None.

## With another 24 hours

Parse the timestamp and latency directly from bytes instead of building strings.
Replace the per-line String keys with small integer ids for hours and endpoints.
Read the input in large chunks instead of one big string.
Measure each change on the board and record it in PERF.md.