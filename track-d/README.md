# Track D (Traverse) - Runtime Terrors

## Approach Overview
We implemented an algorithmic shortest-path solver in high-performance C++20:
- **Zero Dependencies:** Pure standard library + POSIX system calls (`mmap`).
- **CSR Graph Representation:** Compressed Sparse Row format storing 5 million edges contiguously in memory for minimal cache line misses.
- **O(1) State Reset:** Generation-tagged distance array eliminates memory clearing overhead between Dijkstra executions.
- **12-Thread Query Parallelism:** Queries distributed across 12 threads with independent heaps and state vectors.

## Dependencies
None (`dependency_count = 0`).

## What we would do with another 24 hours
Implement Bidirectional Dijkstra and contraction hierarchies preprocessing to drop query latency down to microsecond scale.
