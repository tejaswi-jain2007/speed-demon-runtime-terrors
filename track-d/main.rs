// Speed Demon 2026 - Track D (Traverse)
// Team: Runtime Terrors
// High-efficiency Dijkstra solver: 30MB SoA edges (u32 targets + u16 weights), 6 P-core workers, u32 distance arrays, instant target-pop exit, and non-breaking 64-bit fallback

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

const PROT_READ: i32 = 1;
const MAP_PRIVATE: i32 = 2;
const MADV_SEQUENTIAL: i32 = 2;
const MADV_DONTNEED: i32 = 4;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
}

struct FastMmap {
    ptr: *const u8,
    len: usize,
    _file: File,
}

unsafe impl Send for FastMmap {}
unsafe impl Sync for FastMmap {}

impl FastMmap {
    fn open(path: &str) -> Option<Self> {
        let file = File::open(path).ok()?;
        let len = file.metadata().ok()?.len() as usize;
        if len == 0 { return None; }
        let fd = file.as_raw_fd();
        let ptr = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, fd, 0) };
        if ptr.is_null() || ptr as isize == -1 { return None; }
        unsafe { madvise(ptr, len, MADV_SEQUENTIAL); }
        Some(FastMmap { ptr, len, _file: file })
    }

    #[inline(always)]
    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for FastMmap {
    fn drop(&mut self) {
        unsafe { munmap(self.ptr as *mut u8, self.len); }
    }
}

#[inline(always)]
unsafe fn discard_file_pages(ptr: *const u8, len: usize) {
    let p = ptr as usize;
    let (s, e) = ((p + 4095) & !4095, (p + len) & !4095);
    if e > s {
        madvise(s as *mut u8, e - s, MADV_DONTNEED);
    }
}

struct MinHeap {
    data: Vec<u64>,
}

impl MinHeap {
    fn new() -> Self {
        MinHeap { data: Vec::with_capacity(8192) }
    }

    #[inline(always)]
    fn clear(&mut self) {
        self.data.clear();
    }

    #[inline(always)]
    fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    #[inline(always)]
    fn push(&mut self, dist: u32, u: u32) {
        let val = ((dist as u64) << 32) | (u as u64);
        let mut idx = self.data.len();
        self.data.push(0);
        while idx > 0 {
            let parent = (idx - 1) >> 1;
            let p_val = unsafe { *self.data.get_unchecked(parent) };
            if val < p_val {
                unsafe { *self.data.get_unchecked_mut(idx) = p_val; }
                idx = parent;
            } else {
                break;
            }
        }
        unsafe { *self.data.get_unchecked_mut(idx) = val; }
    }

    #[inline(always)]
    fn pop(&mut self) -> (u32, u32) {
        let min = unsafe { *self.data.get_unchecked(0) };
        let last = self.data.pop().unwrap();
        if !self.data.is_empty() {
            let mut idx = 0;
            let len = self.data.len();
            while (idx << 1) + 1 < len {
                let mut child = (idx << 1) + 1;
                let right = child + 1;
                if right < len && unsafe { *self.data.get_unchecked(right) < *self.data.get_unchecked(child) } {
                    child = right;
                }
                if unsafe { *self.data.get_unchecked(child) < last } {
                    unsafe { *self.data.get_unchecked_mut(idx) = *self.data.get_unchecked(child); }
                    idx = child;
                } else {
                    break;
                }
            }
            unsafe { *self.data.get_unchecked_mut(idx) = last; }
        }
        ((min >> 32) as u32, min as u32)
    }
}

#[inline(always)]
fn get_overflow(index: u32, overflow: &[(u32, u32)]) -> u32 {
    let slot = overflow.binary_search_by_key(&index, |entry| entry.0)
        .expect("overflow entry missing");
    overflow[slot].1
}

// Exact 64-bit fallback for the ~2 injected large-weight queries
fn dijkstra64(
    head: &[u32],
    targets: &[u32],
    weights: &[u16],
    overflow: &[(u32, u32)],
    s: u32,
    t: u32,
) -> i64 {
    let mut d64: HashMap<u32, u64> = HashMap::with_capacity(256);
    let mut pq: BinaryHeap<Reverse<(u64, u32)>> = BinaryHeap::with_capacity(256);
    d64.insert(s, 0);
    pq.push(Reverse((0, s)));
    while let Some(Reverse((d, u))) = pq.pop() {
        if d > *d64.get(&u).unwrap_or(&u64::MAX) { continue; }
        if u == t { return d as i64; }
        let start = head[u as usize] as usize;
        let end = head[(u + 1) as usize] as usize;
        for ei in start..end {
            let next = unsafe { *targets.get_unchecked(ei) };
            let w_comp = unsafe { *weights.get_unchecked(ei) };
            let weight = if w_comp != u16::MAX {
                w_comp as u32
            } else {
                get_overflow(ei as u32, overflow)
            };
            let nd = d.saturating_add(weight as u64);
            let cur = *d64.get(&next).unwrap_or(&u64::MAX);
            if nd < cur {
                d64.insert(next, nd);
                pq.push(Reverse((nd, next)));
            }
        }
    }
    -1
}

#[inline(always)]
fn parse_u32_from_slice(d: &[u8], mut p: usize, end: usize, val: &mut u32) -> usize {
    while p < end && unsafe { *d.get_unchecked(p) } <= b' ' { p += 1; }
    let mut v = 0u32;
    while p < end && unsafe { *d.get_unchecked(p) } >= b'0' {
        v = v * 10 + (unsafe { *d.get_unchecked(p) } - b'0') as u32;
        p += 1;
    }
    *val = v;
    p
}

fn main() {
    let mut data_dir = String::from("/data");
    let mut out_dir = String::from("/out");

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--data" && i + 1 < args.len() {
            data_dir = args[i + 1].clone();
            i += 2;
        } else if args[i] == "--out" && i + 1 < args.len() {
            out_dir = args[i + 1].clone();
            i += 2;
        } else {
            i += 1;
        }
    }

    let graph_path = format!("{}/graph.txt", data_dir);
    let queries_path = format!("{}/queries.txt", data_dir);
    let out_path = format!("{}/distances.txt", out_dir);

    // 1. Read graph.txt
    let g_mmap = match FastMmap::open(&graph_path) {
        Some(m) => m,
        None => return,
    };
    let g_data = g_mmap.as_slice();
    let g_size = g_data.len();

    let mut n: u32 = 0;
    let mut m: u32 = 0;
    let mut gp = 0;
    gp = parse_u32_from_slice(g_data, gp, g_size, &mut n);
    gp = parse_u32_from_slice(g_data, gp, g_size, &mut m);
    let edges_start = gp;

    let num_words = ((n as usize) + 63) >> 6;
    let mut has_in_edges = vec![0u64; num_words];
    let mut head = vec![0u32; (n + 2) as usize];

    // Pass 1: count out-degrees and mark in-edges (skip weights!)
    unsafe {
        let mut p = g_data.as_ptr().add(edges_start);
        for _ in 0..m {
            while *p <= b' ' { p = p.add(1); }
            let mut u = (*p - b'0') as u32; p = p.add(1);
            while *p >= b'0' { u = u * 10 + (*p - b'0') as u32; p = p.add(1); }
            while *p <= b' ' { p = p.add(1); }
            let mut v = (*p - b'0') as u32; p = p.add(1);
            while *p >= b'0' { v = v * 10 + (*p - b'0') as u32; p = p.add(1); }
            while *p <= b' ' { p = p.add(1); }
            while *p >= b'0' { p = p.add(1); }

            head[(u + 1) as usize] += 1;
            *has_in_edges.get_unchecked_mut((v as usize) >> 6) |= 1u64 << ((v as usize) & 63);
        }
    }

    // Prefix sum
    for j in 0..=n as usize {
        head[j + 1] += head[j];
    }

    let mut targets = vec![0u32; m as usize];
    let mut weights = vec![0u16; m as usize];
    let mut overflow = Vec::<(u32, u32)>::with_capacity(64);

    // Pass 2: populate CSR edges
    unsafe {
        let mut p = g_data.as_ptr().add(edges_start);
        for _ in 0..m {
            while *p <= b' ' { p = p.add(1); }
            let mut u = (*p - b'0') as u32; p = p.add(1);
            while *p >= b'0' { u = u * 10 + (*p - b'0') as u32; p = p.add(1); }
            while *p <= b' ' { p = p.add(1); }
            let mut v = (*p - b'0') as u32; p = p.add(1);
            while *p >= b'0' { v = v * 10 + (*p - b'0') as u32; p = p.add(1); }
            while *p <= b' ' { p = p.add(1); }
            let mut w = (*p - b'0') as u32; p = p.add(1);
            while *p >= b'0' { w = w * 10 + (*p - b'0') as u32; p = p.add(1); }

            let idx = head[u as usize] as usize;
            head[u as usize] += 1;
            *targets.get_unchecked_mut(idx) = v;
            if w >= u16::MAX as u32 {
                *weights.get_unchecked_mut(idx) = u16::MAX;
                overflow.push((idx as u32, w));
            } else {
                *weights.get_unchecked_mut(idx) = w as u16;
            }
        }
    }

    // Discard graph.txt memory immediately
    unsafe { discard_file_pages(g_data.as_ptr(), g_size); }
    drop(g_mmap);

    // Restore head offsets
    for j in (0..n as usize).rev() {
        head[j + 1] = head[j];
    }
    head[0] = 0;
    head.truncate((n + 1) as usize);
    overflow.sort_unstable_by_key(|entry| entry.0);

    // Read queries
    let q_mmap = match FastMmap::open(&queries_path) {
        Some(m) => m,
        None => return,
    };
    let q_data = q_mmap.as_slice();
    let q_size = q_data.len();
    let mut queries = Vec::with_capacity(2048);
    let mut qp = 0;
    while qp < q_size {
        let mut s = 0u32;
        let mut t = 0u32;
        qp = parse_u32_from_slice(q_data, qp, q_size, &mut s);
        if qp >= q_size { break; }
        qp = parse_u32_from_slice(q_data, qp, q_size, &mut t);
        queries.push((s, t));
    }
    drop(q_mmap);

    let num_queries = queries.len();
    if num_queries == 0 {
        let _ = std::fs::create_dir_all(&out_dir);
        let _ = File::create(&out_path);
        return;
    }

    let num_workers = 6.min(num_queries).max(1);
    let head_arc = Arc::new(head);
    let targets_arc = Arc::new(targets);
    let weights_arc = Arc::new(weights);
    let overflow_arc = Arc::new(overflow);
    let has_in_arc = Arc::new(has_in_edges);
    let queries_arc = Arc::new(queries);
    let next_q_arc = Arc::new(AtomicUsize::new(0));

    let mut all_answers = vec![0i64; num_queries];
    let answers_ptr = all_answers.as_mut_ptr() as usize;

    let mut workers = Vec::with_capacity(num_workers);

    for _ in 0..num_workers {
        let head_ref = Arc::clone(&head_arc);
        let targets_ref = Arc::clone(&targets_arc);
        let weights_ref = Arc::clone(&weights_arc);
        let overflow_ref = Arc::clone(&overflow_arc);
        let in_ref = Arc::clone(&has_in_arc);
        let queries_ref = Arc::clone(&queries_arc);
        let next_q = Arc::clone(&next_q_arc);

        let builder = thread::Builder::new().stack_size(256 * 1024);
        workers.push(builder.spawn(move || {
            let out_ptr = answers_ptr as *mut i64;
            // 32-bit distance array: handles all normal distances with zero hashmap overhead
            let mut distances = vec![u32::MAX; head_ref.len() - 1];
            let mut touched: Vec<u32> = Vec::with_capacity(16384);
            let mut pq = MinHeap::new();

            loop {
                let qi = next_q.fetch_add(1, Ordering::Relaxed);
                if qi >= queries_ref.len() {
                    break;
                }

                let (s, t) = queries_ref[qi];

                if s == t {
                    unsafe { *out_ptr.add(qi) = 0; }
                    continue;
                }

                let t_has_in = (unsafe { *in_ref.get_unchecked((t as usize) >> 6) } & (1u64 << ((t as usize) & 63))) != 0;
                let s_begin = unsafe { *head_ref.get_unchecked(s as usize) };
                let s_end = unsafe { *head_ref.get_unchecked((s + 1) as usize) };
                if !t_has_in || s_begin == s_end {
                    unsafe { *out_ptr.add(qi) = -1; }
                    continue;
                }

                unsafe { *distances.get_unchecked_mut(s as usize) = 0; }
                touched.push(s);
                pq.push(0, s);

                let mut best_t = u32::MAX;
                let mut overflow_64 = false;

                while !pq.is_empty() {
                    let (d, u) = pq.pop();
                    if d >= best_t {
                        break;
                    }
                    let cur_u = unsafe { *distances.get_unchecked(u as usize) };
                    if d > cur_u {
                        continue;
                    }
                    if u == t {
                        best_t = d;
                        break;
                    }

                    let start = unsafe { *head_ref.get_unchecked(u as usize) as usize };
                    let end = unsafe { *head_ref.get_unchecked((u + 1) as usize) as usize };
                    for ei in start..end {
                        let next = unsafe { *targets_ref.get_unchecked(ei) };
                        let w_comp = unsafe { *weights_ref.get_unchecked(ei) };
                        let weight = if w_comp != u16::MAX {
                            w_comp as u32
                        } else {
                            get_overflow(ei as u32, &overflow_ref)
                        };

                        let (nd, ovf) = d.overflowing_add(weight);
                        if ovf {
                            overflow_64 = true;
                            continue;
                        }
                        if nd >= best_t {
                            continue;
                        }
                        if next == t {
                            best_t = nd;
                            continue;
                        }

                        let cur = unsafe { *distances.get_unchecked(next as usize) };
                        if nd < cur {
                            if cur == u32::MAX {
                                touched.push(next);
                            }
                            unsafe { *distances.get_unchecked_mut(next as usize) = nd; }
                            pq.push(nd, next);
                        }
                    }
                }

                for &u in &touched {
                    unsafe { *distances.get_unchecked_mut(u as usize) = u32::MAX; }
                }
                touched.clear();
                pq.clear();

                let ans = if best_t != u32::MAX {
                    best_t as i64
                } else if overflow_64 {
                    dijkstra64(&head_ref, &targets_ref, &weights_ref, &overflow_ref, s, t)
                } else {
                    -1
                };

                unsafe { *out_ptr.add(qi) = ans; }
            }
        }).unwrap());
    }

    for w in workers {
        w.join().unwrap();
    }

    // Output distances.txt
    let mut out_buffer = Vec::with_capacity(32 * 1024);
    for d in all_answers {
        if d < 0 {
            out_buffer.extend_from_slice(b"-1\n");
        } else {
            let mut temp = [0u8; 20];
            let mut idx = 20;
            let mut v = d as u64;
            if v == 0 {
                out_buffer.extend_from_slice(b"0\n");
            } else {
                while v > 0 {
                    idx -= 1;
                    temp[idx] = b'0' + (v % 10) as u8;
                    v /= 10;
                }
                out_buffer.extend_from_slice(&temp[idx..]);
                out_buffer.push(b'\n');
            }
        }
    }

    let _ = std::fs::create_dir_all(&out_dir);
    if let Ok(mut f) = File::create(&out_path) {
        let _ = f.write_all(&out_buffer);
    }
}
