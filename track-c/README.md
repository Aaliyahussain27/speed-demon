# Track C: Join

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/c-join.md).

## Approach

Stage 1: initial rust scaffold

Stage 2:
Region lookup as an array. Region ids go up to 999,999, so a Vec<u16> indexed by id gives the country slot in one memory read. No hashing per record.
Parallel chunks. Each thread gets a slice of the file. A byte is inside quotes when an odd number of " bytes come before it, so each thread counts quotes in its slice, a prefix XOR gives every slice its starting quote state, and each slice finds its first record start.
Hand-written parser. It follows spec section 2.1: optional BOM at file start or after a record terminator, CRLF or LF terminators outside quotes, "" escapes, and LF or comma inside quotes.
Money as integer cents. -5.25 becomes -525. Printing adds the sign and the two decimals, and never prints -0.00.
Distinct names. Each thread gives each new name a local id. The ids are merged into one global id space, and each country keeps a bitset of the ids it has seen. The distinct count is the number of set bits.
Output. Countries are sorted by code bytes, and _UNMATCHED comes last.

## Dependencies

None.

## With another 24 hours

Check the full dataset hash, then measure the LTO profile on and off on the board.
Replace the default hasher for name ids with a simpler, faster one, and measure it.
Remove the per-name copies made during the merge step.
Try profile-guided optimization (PGO) as its own stage.