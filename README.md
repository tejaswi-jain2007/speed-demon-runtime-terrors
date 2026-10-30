# Speed Demon 2026 - Runtime Terrors ⚡

**Team:** Runtime Terrors  
**Members:** Tejaswi Jain, Arohi Nigam  
**Repository:** [https://github.com/tejaswi-jain2007/speed-demon-runtime-terrors](https://github.com/tejaswi-jain2007/speed-demon-runtime-terrors)  
**Branch:** `speed-demon`

---

## Championship Submission Overview
We are competing for the **Overall Speed Demon Championship** across all four tracks with ultra-fast, zero-dependency Rust engines designed specifically for the Hetzner Falkenstein hardware (Intel Core i5-13500 pinned to 6 P-cores / 12 hardware threads).

| Track | Task | Language | Architecture Highlights |
|---|---|---|---|
| **Track A (Ingest)** | 3.1M log line hourly stats & percentiles | Rust | Zero-alloc `mmap` parser, 12-thread parallel chunking, in-place pdqsort percentiles |
| **Track B (Index)** | 50,000 boolean & phrase queries over corpus | Rust | Vocabulary-filtered index, sorted posting lists, two-pointer linear set operations |
| **Track C (Join)** | Multi-table CSV join & distinct aggregation | Rust | Zero-alloc streaming CSV state machine, flat region lookup, 64-bit FNV distinct hashes |
| **Track D (Traverse)** | 5M edge graph, 2,000 shortest-path queries | Rust | Compact forward-only CSR (~44MB), generation-stamped $O(1)$ reset, 12-thread parallel Dijkstra |

---

## Design Principles
1. **Zero External Dependencies ($D = 1$):**
   - Built exclusively with standard Rust (`dependency_count = 0`).
   - Compliant with offline build requirements (`--network=none`).
2. **Mechanical Sympathy:**
   - Pre-allocated flat buffers for zero dynamic heap allocations in hot loops.
   - Cache-conscious data structures (compact CSR arrays, linear postings).
3. **Hardware Saturation:**
   - Scaled to 12 hardware threads across 6 physical P-cores with lock-free independent workers.
