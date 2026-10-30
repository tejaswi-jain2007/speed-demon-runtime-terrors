# Track A (Ingest) - Runtime Terrors

## Approach Overview
We implemented an ultra-fast, zero-dependency log ingestion engine written in standard C++20:
- **Zero Dependencies:** Pure standard library + POSIX system calls (`mmap`, `stat`).
- **Parallel Execution:** File is sliced into 12 chunks aligned to newlines, matching the 6 P-cores (12 threads) of the target Hetzner Intel Core i5-13500 machine.
- **Fast Parsing:** Direct ASCII arithmetic parsing for integers and timestamps; endpoint strings referenced directly from memory map.
- **Efficient Aggregations:** Thread-local hash maps merged into a canonical ordered map, followed by parallel percentile sorting and streamed buffer output.

## Dependencies
None (`dependency_count = 0`).

## What we would do with another 24 hours
Implement SIMD / AVX2 SWAR parsing for newline and space discovery across 32-byte registers to push parse time under 500 milliseconds.
