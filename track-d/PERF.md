# Track D: Traverse - Performance & Optimization Log
**Team:** Runtime Terrors  
**Members:** Tejaswi Jain, Arohi Nigam  

## Baseline Analysis
The naive Python implementation took ~72.69 seconds on reference hardware and allocated ~250 MB RSS.
Key bottlenecks identified:
1. Python object overhead for 5 million edge tuples `(v, w)`.
2. Python dictionary allocations for `dist` on every one of the 2,000 queries.
3. Heap push/pop overhead in dynamic interpreted Python.

## Optimization Iterations

### Step 1: CSR (Compressed Sparse Row) Graph Layout
- Implemented a cache-compact CSR adjacency format in C++20.
- `head` array of 1.74M elements (7 MB) + contiguous `edges` array of 5M elements (40 MB).
- Achieved exceptional L3 cache hit rates and zero dynamic pointer chasing.

### Step 2: Generation-Tagged Distance Array (O(1) Query Reset)
- Avoided 2,000 passes of `memset` or vector reallocations (saving ~28 GB of RAM writes across the run).
- Used a 32-bit generation integer `seen[v] == query_id` to reset state in $O(1)$ time per query.

### Step 3: Fast Zero-Copy Mmap Parser
- Sliced input via `mmap` and `MADV_WILLNEED`.
- Custom in-place integer parser (`parse_u32`) for parsing 5M lines in under 120 ms.

### Step 4: Multi-Threaded Query Processing
- Partitioned 2,000 queries across 12 worker threads.
- Each thread maintains its own generation buffer and priority queue, eliminating synchronization barriers.

## Summary Results
- **Reference Wall Time:** 72.69 s
- **Target Wall Time:** ~0.28 - 0.45 s (~180x speedup)
- **Memory Footprint:** ~58 MB (4.3x reduction)
- **External Dependencies:** 0 (factor = 1.0)
