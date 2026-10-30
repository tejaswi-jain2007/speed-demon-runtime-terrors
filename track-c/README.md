# Track C: Join - Runtime Terrors

## Approach
Our implementation is a high-performance zero-dependency streaming CSV join and aggregation engine in C++20:
- **Zero Dependencies:** Pure C++20 standard library with POSIX system calls (`mmap`, `madvise`).
- **Direct $O(1)$ Region Lookup Table:** Maps region IDs directly to compact country IDs via a flat 10M array, bypassing hash map lookups.
- **Fast Hostile CSV Parser:** Linear state machine seamlessly handles quotes, embedded commas, RFC4180 `""` quote escapes, multiline notes with bare LFs, and mid-file BOM sequences.
- **Zero-Allocation Aggregations:** Computes exact integer cents, note byte counts, and unescaped name lengths with zero unnecessary memory allocations.

## Dependencies
None (`dependency_count = 0`).

## With another 24 hours
Implement SIMD delimiter scanning using AVX-512 to parse unquoted fields at 16 bytes per instruction.
