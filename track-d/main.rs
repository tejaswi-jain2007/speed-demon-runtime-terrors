// Speed Demon 2026 - Track D (Traverse) - Ultra High Performance
// Team: Runtime Terrors
// Parallel 16-thread CSR loader, lock-free slot partitioning, 10-core Dijkstra pool with 3.48MB u16 distance cache, zero-allocation exit

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
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
    fn _exit(status: i32) -> !;
}

#[derive(Clone, Copy)]
struct Edge {
    to: u32,
    weight: u32,
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

#[inline(always)]
fn parse_u32(d: &[u8], mut p: usize, val: &mut u32) -> usize {
    while p < d.len() && unsafe { *d.get_unchecked(p) } <= b' ' { p += 1; }
    let mut v = 0u32;
    while p < d.len() && unsafe { *d.get_unchecked(p) } >= b'0' {
        v = v * 10 + (unsafe { *d.get_unchecked(p) } - b'0') as u32;
        p += 1;
    }
    *val = v;
    p
}

#[derive(Clone, Copy)]
struct SyncEdges(*mut Edge);
unsafe impl Send for SyncEdges {}
unsafe impl Sync for SyncEdges {}

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

// 64-bit fallback for rare large-weight queries
fn dijkstra64(head: &[u32], edges: &[Edge], s: u32, t: u32) -> i64 {
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
            let e = unsafe { edges.get_unchecked(ei) };
            let nd = d.saturating_add(e.weight as u64);
            let cur = *d64.get(&e.to).unwrap_or(&u64::MAX);
            if nd < cur {
                d64.insert(e.to, nd);
                pq.push(Reverse((nd, e.to)));
            }
        }
    }
    -1
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

    // 1. Read graph.txt in parallel
    let g_mmap = match FastMmap::open(&graph_path) {
        Some(m) => m,
        None => return,
    };
    let g_data = g_mmap.as_slice();

    let mut n = 0u32;
    let mut m = 0u32;
    let mut gp = 0;
    gp = parse_u32(g_data, gp, &mut n);
    gp = parse_u32(g_data, gp, &mut m);
    let edges_start = gp;

    let num_load_threads = 16;
    let edges_len = g_data.len() - edges_start;
    let chunk_size = edges_len / num_load_threads;
    let mut bounds = vec![edges_start; num_load_threads + 1];
    for t in 1..num_load_threads {
        let mut target = edges_start + t * chunk_size;
        while target < g_data.len() && g_data[target - 1] != b'\n' {
            target += 1;
        }
        bounds[t] = target;
    }
    bounds[num_load_threads] = g_data.len();

    let num_words = ((n as usize) + 63) >> 6;
    let has_in_edges = (0..num_words).map(|_| AtomicU64::new(0)).collect::<Vec<_>>();
    let head = (0..=(n + 1)).map(|_| AtomicU32::new(0)).collect::<Vec<_>>();

    // Pass 1: degree counting and incoming edge flags
    thread::scope(|s| {
        for t in 0..num_load_threads {
            let start = bounds[t];
            let end = bounds[t + 1];
            let head_ref = &head;
            let in_ref = &has_in_edges;
            s.spawn(move || {
                let mut p = unsafe { g_data.as_ptr().add(start) };
                let end_p = unsafe { g_data.as_ptr().add(end) };
                while p < end_p {
                    while unsafe { *p } <= b' ' { p = unsafe { p.add(1) }; }
                    if p >= end_p { break; }
                    let mut u = unsafe { (*p - b'0') as u32 }; p = unsafe { p.add(1) };
                    while unsafe { *p } >= b'0' { u = u * 10 + unsafe { (*p - b'0') as u32 }; p = unsafe { p.add(1) }; }
                    while unsafe { *p } <= b' ' { p = unsafe { p.add(1) }; }
                    let mut v = unsafe { (*p - b'0') as u32 }; p = unsafe { p.add(1) };
                    while unsafe { *p } >= b'0' { v = v * 10 + unsafe { (*p - b'0') as u32 }; p = unsafe { p.add(1) }; }
                    while unsafe { *p } != b'\n' { p = unsafe { p.add(1) }; }
                    p = unsafe { p.add(1) };

                    head_ref[(u + 1) as usize].fetch_add(1, Ordering::Relaxed);
                    in_ref[(v as usize) >> 6].fetch_or(1u64 << ((v as usize) & 63), Ordering::Relaxed);
                }
            });
        }
    });

    // Prefix sum in-place
    let mut current_offset = 0u32;
    for i in 0..=n as usize {
        let deg = head[i + 1].load(Ordering::Relaxed);
        head[i].store(current_offset, Ordering::Relaxed);
        current_offset += deg;
    }
    head[(n + 1) as usize].store(current_offset, Ordering::Relaxed);

    let mut edges = vec![Edge { to: 0, weight: 0 }; m as usize];
    let edges_ptr = SyncEdges(edges.as_mut_ptr());

    // Pass 2: populate CSR edges concurrently
    thread::scope(|s| {
        for t in 0..num_load_threads {
            let start = bounds[t];
            let end = bounds[t + 1];
            let head_ref = &head;
            let ep = edges_ptr;
            s.spawn(move || {
                let mut p = unsafe { g_data.as_ptr().add(start) };
                let end_p = unsafe { g_data.as_ptr().add(end) };
                while p < end_p {
                    while unsafe { *p } <= b' ' { p = unsafe { p.add(1) }; }
                    if p >= end_p { break; }
                    let mut u = unsafe { (*p - b'0') as u32 }; p = unsafe { p.add(1) };
                    while unsafe { *p } >= b'0' { u = u * 10 + unsafe { (*p - b'0') as u32 }; p = unsafe { p.add(1) }; }
                    while unsafe { *p } <= b' ' { p = unsafe { p.add(1) }; }
                    let mut v = unsafe { (*p - b'0') as u32 }; p = unsafe { p.add(1) };
                    while unsafe { *p } >= b'0' { v = v * 10 + unsafe { (*p - b'0') as u32 }; p = unsafe { p.add(1) }; }
                    while unsafe { *p } <= b' ' { p = unsafe { p.add(1) }; }
                    let mut w = unsafe { (*p - b'0') as u32 }; p = unsafe { p.add(1) };
                    while unsafe { *p } >= b'0' { w = w * 10 + unsafe { (*p - b'0') as u32 }; p = unsafe { p.add(1) }; }
                    while unsafe { *p } != b'\n' && p < end_p { p = unsafe { p.add(1) }; }
                    p = unsafe { p.add(1) };

                    let slot = head_ref[u as usize].fetch_add(1, Ordering::Relaxed) as usize;
                    unsafe { *ep.0.add(slot) = Edge { to: v, weight: w }; }
                }
            });
        }
    });

    unsafe { discard_file_pages(g_data.as_ptr(), g_data.len()); }
    drop(g_mmap);

    // Restore head offsets
    let mut final_head = vec![0u32; (n + 2) as usize];
    final_head[0] = 0;
    for j in 0..n as usize {
        final_head[j + 1] = head[j].load(Ordering::Relaxed);
    }
    final_head[(n + 1) as usize] = current_offset;
    drop(head);

    let in_bits = has_in_edges.into_iter().map(|a| a.load(Ordering::Relaxed)).collect::<Vec<_>>();

    // 2. Read queries
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
        qp = parse_u32(q_data, qp, &mut s);
        if qp >= q_size { break; }
        qp = parse_u32(q_data, qp, &mut t);
        queries.push((s, t));
    }
    drop(q_mmap);

    let num_queries = queries.len();
    if num_queries == 0 {
        let _ = std::fs::create_dir_all(&out_dir);
        let _ = File::create(&out_path);
        return;
    }

    let num_workers = 10.min(num_queries).max(1);
    let head_arc = Arc::new(final_head);
    let edges_arc = Arc::new(edges);
    let in_arc = Arc::new(in_bits);
    let queries_arc = Arc::new(queries);
    let next_q_arc = Arc::new(AtomicUsize::new(0));

    let mut all_answers = vec![0i64; num_queries];
    let answers_ptr = all_answers.as_mut_ptr() as usize;

    let mut workers = Vec::with_capacity(num_workers);

    for _ in 0..num_workers {
        let head_ref = Arc::clone(&head_arc);
        let edges_ref = Arc::clone(&edges_arc);
        let in_ref = Arc::clone(&in_arc);
        let queries_ref = Arc::clone(&queries_arc);
        let next_q = Arc::clone(&next_q_arc);

        let builder = thread::Builder::new().stack_size(256 * 1024);
        workers.push(builder.spawn(move || {
            let out_ptr = answers_ptr as *mut i64;
            let mut dist16 = vec![0u16; head_ref.len() - 1];
            let mut touched: Vec<u32> = Vec::with_capacity(8192);
            let mut pq = MinHeap::new();

            loop {
                let qi = next_q.fetch_add(1, Ordering::Relaxed);
                if qi >= queries_ref.len() { break; }
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

                unsafe { *dist16.get_unchecked_mut(s as usize) = 1; }
                touched.push(s);
                pq.push(0, s);

                let mut best_t = u32::MAX;

                while !pq.is_empty() {
                    let (d, u) = pq.pop();
                    if d >= best_t { break; }
                    let cur_u_stored = unsafe { *dist16.get_unchecked(u as usize) as u32 };
                    if cur_u_stored == 0 || d > cur_u_stored - 1 { continue; }
                    if u == t {
                        best_t = d;
                        break;
                    }

                    let start = unsafe { *head_ref.get_unchecked(u as usize) as usize };
                    let end = unsafe { *head_ref.get_unchecked((u + 1) as usize) as usize };
                    for ei in start..end {
                        let e = unsafe { edges_ref.get_unchecked(ei) };
                        let nd = d.saturating_add(e.weight);
                        if nd >= best_t || nd >= 65534 { continue; }
                        if e.to == t {
                            best_t = nd;
                            continue;
                        }

                        let cur_stored = unsafe { *dist16.get_unchecked(e.to as usize) as u32 };
                        if cur_stored == 0 || nd < cur_stored - 1 {
                            if cur_stored == 0 { touched.push(e.to); }
                            unsafe { *dist16.get_unchecked_mut(e.to as usize) = (nd + 1) as u16; }
                            pq.push(nd, e.to);
                        }
                    }
                }

                for &u in &touched {
                    unsafe { *dist16.get_unchecked_mut(u as usize) = 0; }
                }
                touched.clear();
                pq.clear();

                let ans = if best_t != u32::MAX {
                    best_t as i64
                } else {
                    dijkstra64(&head_ref, &edges_ref, s, t)
                };

                unsafe { *out_ptr.add(qi) = ans; }
            }
        }).unwrap());
    }

    for w in workers {
        w.join().unwrap();
    }

    // 3. Fast output generation
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
    unsafe { _exit(0); }
}
