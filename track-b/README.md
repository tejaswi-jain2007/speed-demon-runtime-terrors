# Track B (Index) - Runtime Terrors

## Approach Overview
We engineered a high-throughput search and index evaluation engine in C++20:
- **Zero Dependencies:** Pure standard library + POSIX system calls (`mmap`).
- **Sorted Postings Inverted Index:** Inverted index storing sequential `uint32_t` doc IDs.
- **Fast Linear Set Intersections:** Evaluates boolean queries (AND, OR, NOT) using cache-friendly linear set operations.
- **12-Thread Query Parallelism:** Slices the 50,000 queries evenly across 12 worker threads.

## Dependencies
None (`dependency_count = 0`).

## What we would do with another 24 hours
Implement SIMD bit-packing / roaring bitmaps for posting list intersections to reduce memory bandwidth by another 40%.
