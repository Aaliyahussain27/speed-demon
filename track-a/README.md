# Track A: Ingest

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

Stage 1: 
1. Read the whole log into memory.
2. For each line, split it on spaces and keep the timestamp, endpoint, status and latency.
3. The hour is the first 13 characters of the timestamp, plus :00:00Z.
4. Store each (hour, endpoint) box in a BTreeMap. Each box keeps an error count and a Vec of latencies.
5. Sort each latency list. Read p50, p95 and p99 with the nearest-rank formula (p * n + 99) / 100.
6. Error rate is errors * 1000000 / count, in integers.
7. Write one line per box, in key order.

Stage 2. The program reads the log once, through a 1 MB buffer, so the whole file is never held in memory as text.

1. Each line is split into fields as byte slices. Nothing is copied into new strings.
2. The timestamp gives the hour. Day and hour digits are read from fixed positions and turned into a slot from 0 to 95 (day ordinal × 24 + hour).
3. Status and latency are read as digits, byte by byte.
4. Each endpoint gets a small integer id the first time it appears. A `HashMap` from endpoint bytes to id is used only for that lookup.
5. Each (endpoint, hour) box lives in one flat `Vec`, at index `endpoint_id * 96 + hour_slot`. A box keeps an error count and a `Vec<u32>` of latencies.
6. At the end, each box's latencies are sorted, and p50, p95 and p99 are read with the nearest-rank formula `(p * n + 99) / 100`.
7. Error rate is `errors * 1000000 / count`, in integers.
8. Output goes hour by hour, then endpoint by its name's bytes, so the order matches the spec.

## Dependencies

None.

## With another 24 hours

- Replace the `HashMap` endpoint lookup with a faster hash or a precomputed table.
- Measure each change on its own, with a board run ID for each row in PERF.md.
- Reduce source lines where the code repeats itself, since source size counts toward the score.