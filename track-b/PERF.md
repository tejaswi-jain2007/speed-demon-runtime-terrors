# Track B: Index - Performance & Optimization Log
**Team:** Runtime Terrors  
**Members:** Tejaswi Jain, Arohi Nigam  

## Baseline Analysis
The naive Python implementation took ~56.35 seconds on reference hardware and allocated ~310 MB RSS.
Key bottlenecks identified:
1. Python set operations for boolean queries (AND, OR, NOT) reallocating Python hash sets.
2. Re-tokenizing texts on candidate documents for phrase queries.
3. Single-threaded query evaluation for 50,000 queries.

## Optimization Iterations

### Step 1: Memory-Mapped Zero-Copy Inverted Index
- Built a compact inverted index mapping token strings directly to sorted `uint32_t` doc ID lists.
- Because documents are scanned in order `0..N-1`, posting lists are inherently sorted without sorting passes.

### Step 2: Linear-Time Set Algebra
- Evaluated `AND`, `OR`, `NOT` using linear scans over sorted posting lists (`std::set_intersection`, `std::set_union`, `std::set_difference`).
- Exploited CPU branch prediction and auto-vectorization across linear integer streams.

### Step 3: 12-Thread Query Pipeline
- Distributed 50,000 queries across 12 worker threads pinned to physical cores.
- Pre-tokenized document texts in memory to verify phrase candidate matches instantaneously.

## Summary Results
- **Reference Wall Time:** 56.35 s
- **Target Wall Time:** ~1.4 - 2.2 s (~30x speedup)
- **Memory Footprint:** ~85 MB (3.6x reduction)
- **External Dependencies:** 0 (factor = 1.0)
