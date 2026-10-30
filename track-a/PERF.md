# Track A: Ingest - Performance & Optimization Log
**Team:** Runtime Terrors  
**Members:** Tejaswi Jain, Arohi Nigam  

## Baseline Analysis
The naive Python implementation took ~32.05 seconds on reference hardware and allocated ~228 MB RSS.
Key bottlenecks identified:
1. `datetime.strptime()` parsing RFC3339 timestamps for 3.1M records in Python.
2. String splitting and object allocation overhead (over 15 million Python heap objects).
3. Sorting millions of latencies dynamically per group in Python.

## Optimization Iterations

### Step 1: Zero-Dependency Modern C++ Engine
- Replaced Python reference with zero-dependency C++20 (`dependency_count = 0`).
- Direct memory-mapped I/O (`mmap`) with `MADV_WILLNEED` to eliminate kernel-to-user space copy overheads.
- Reduced memory footprint from 228 MB down to ~35 MB.

### Step 2: Custom Zero-Allocation ASCII Parser
- Timestamp format is strictly fixed: the first 13 bytes (`YYYY-MM-DDTHH`) fully determine the hourly bucket. Bypassed date parsing logic entirely.
- In-place parsing for endpoints and latencies directly from the `mmap` pointer without allocations.
- Fixed 3-digit status code evaluation (`500 <= status <= 599`).

### Step 3: Multi-Core Scalability (12 Threads on 6 P-Cores)
- Partitioned the 239 MB log file into 12 contiguous chunks along newline boundaries.
- Thread-local grouping maps to eliminate lock contention during parsing.
- Merged thread-local summaries into a sorted total order `(hour, endpoint)` container.

### Step 4: Parallel Percentile Resolution & Streamed I/O
- Distributed percentile calculation across worker threads:
  - Sorted latency vectors in-place using `std::sort`.
  - Computed nearest-rank percentiles `(p * n + 99) / 100` without auxiliary arrays.
- Emitted output using a double-buffered 4MB block writer to maximize throughput to `/out/summary.txt`.

## Summary Results
- **Reference Wall Time:** 32.05 s
- **Target Wall Time:** ~1.1 - 1.3 s (~25x speedup)
- **Memory Footprint:** ~38 MB (6x reduction)
- **External Dependencies:** 0 (factor = 1.0)
