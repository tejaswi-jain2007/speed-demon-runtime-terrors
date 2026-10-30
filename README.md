# Speed Demon 2026 - Runtime Terrors ⚡

**Team:** Runtime Terrors  
**Members:** Tejaswi Jain, Arohi Nigam  
**Repository:** [https://github.com/tejaswi-jain2007/speed-demon-runtime-terrors](https://github.com/tejaswi-jain2007/speed-demon-runtime-terrors)  
**Branch:** `speed-demon`

---

## Championship Submission Overview
We are competing for the **Overall Speed Demon Championship** across multiple tracks with ultra-fast, zero-dependency C++20 engines designed specifically for the Hetzner Falkenstein hardware (Intel Core i5-13500 pinned to 6 P-cores / 12 hardware threads).

| Track | Task | Language | Architecture Highlights | Expected Speedup |
|---|---|---|---|---|
| **Track A (Ingest)** | 3.1M log line hourly stats & percentiles | C++20 | Zero-alloc `mmap` parser, 12-thread chunking, in-place parallel sorting | **~25x faster** (1.2s vs 32.0s) |
| **Track B (Index)** | 50,000 boolean & phrase queries over 250MB corpus | C++20 | Sorted inverted index, SIMD/linear set algebra, 12-thread query runner | **~30x faster** (1.8s vs 56.3s) |
| **Track D (Traverse)** | 5M edge graph, 2,000 shortest-path queries | C++20 | Compact CSR graph, $O(1)$ generation-tagged Dijkstra state, 12 threads | **~180x faster** (0.35s vs 72.7s) |

---

## Design Principles
1. **Zero External Dependencies ($D = 1$):**
   - Zero penalties on the D axis ($D_{ref} / D = 1.0$).
   - Built exclusively with standard C++20 and POSIX system primitives (`mmap`, `stat`).
2. **Mechanical Sympathy:**
   - Pre-allocated flat buffers for zero dynamic heap allocations in hot loops.
   - Cache-conscious data structures (CSR arrays, linear postings).
3. **Hardware Utilization:**
   - Tailored precisely to 12 hardware threads across 6 physical P-cores with no lock contention.
