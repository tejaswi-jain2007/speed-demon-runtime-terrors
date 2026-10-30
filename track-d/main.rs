// Speed Demon 2026 - Track D (Traverse)
// Team: Runtime Terrors
// Ultra-fast zero-dependency Forward-Only 4-ary MinHeap Dijkstra solver in pure Rust

use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use std::thread;

const PROT_READ: i32 = 1;
const MAP_PRIVATE: i32 = 2;
const MADV_WILLNEED: i32 = 3;

extern "C" {
    fn mmap(addr: *mut u8, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn munmap(addr: *mut u8, length: usize) -> i32;
    fn madvise(addr: *mut u8, length: usize, advice: i32) -> i32;
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
        if len == 0 {
            return None;
        }
        let fd = file.as_raw_fd();
        let ptr = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, fd, 0) };
        if ptr == std::ptr::null_mut() || ptr as isize == -1 {
            return None;
        }
        unsafe {
            madvise(ptr, len, MADV_WILLNEED);
        }
        Some(FastMmap { ptr, len, _file: file })
    }

    #[inline(always)]
    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for FastMmap {
    fn drop(&mut self) {
        unsafe {
            munmap(self.ptr as *mut u8, self.len);
        }
    }
}

#[derive(Clone, Copy)]
struct Edge {
    to: u32,
    weight: u32,
}

#[derive(Clone, Copy)]
struct EdgeTriple {
    u: u32,
    v: u32,
    w: u32,
}

// 4-ary Min-Heap for maximum CPU L1 cache line efficiency
struct MinHeap {
    data: Vec<(u64, u32)>, // (dist, node)
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
    fn push(&mut self, dist: u64, u: u32) {
        let mut idx = self.data.len();
        self.data.push((dist, u));
        while idx > 0 {
            let parent = (idx - 1) >> 2;
            if unsafe { self.data.get_unchecked(idx).0 < self.data.get_unchecked(parent).0 } {
                self.data.swap(idx, parent);
                idx = parent;
            } else {
                break;
            }
        }
    }

    #[inline(always)]
    fn pop(&mut self) -> (u64, u32) {
        let min = self.data[0];
        let last = self.data.pop().unwrap();
        if !self.data.is_empty() {
            self.data[0] = last;
            let mut idx = 0;
            let len = self.data.len();
            loop {
                let mut best = idx;
                let child = (idx << 2) + 1;
                let end = (child + 4).min(len);
                for c in child..end {
                    if unsafe { self.data.get_unchecked(c).0 < self.data.get_unchecked(best).0 } {
                        best = c;
                    }
                }
                if best != idx {
                    self.data.swap(idx, best);
                    idx = best;
                } else {
                    break;
                }
            }
        }
        min
    }
}

#[inline(always)]
fn parse_u32(data: &[u8], mut p: usize, end: usize, val: &mut u32) -> usize {
    while p < end && (data[p] == b' ' || data[p] == b'\n' || data[p] == b'\r') {
        p += 1;
    }
    let mut res: u64 = 0;
    while p < end && data[p] >= b'0' && data[p] <= b'9' {
        res = res * 10 + (data[p] - b'0') as u64;
        p += 1;
    }
    *val = res as u32;
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
        Some(m) => Arc::new(m),
        None => return,
    };
    let g_data = g_mmap.as_slice();
    let g_size = g_data.len();

    let mut n: u32 = 0;
    let mut m: u32 = 0;
    let mut gp = 0;
    gp = parse_u32(g_data, gp, g_size, &mut n);
    gp = parse_u32(g_data, gp, g_size, &mut m);
    let edges_start_offset = gp;

    // Parallel multi-threaded chunk parsing for graph.txt
    let parse_threads = 12.min(thread::available_parallelism().map(|t| t.get()).unwrap_or(12)).max(1);
    let mut chunk_offsets = vec![edges_start_offset; parse_threads + 1];
    chunk_offsets[parse_threads] = g_size;
    let payload = g_size - edges_start_offset;

    for t in 1..parse_threads {
        let mut off = edges_start_offset + t * (payload / parse_threads);
        while off < g_size && g_data[off] != b'\n' {
            off += 1;
        }
        if off < g_size {
            off += 1;
        }
        chunk_offsets[t] = off;
    }

    let mut parse_handles = Vec::with_capacity(parse_threads);
    for t in 0..parse_threads {
        let g_ref = Arc::clone(&g_mmap);
        let start = chunk_offsets[t];
        let end = chunk_offsets[t + 1];
        parse_handles.push(thread::spawn(move || {
            let d = g_ref.as_slice();
            let mut edges = Vec::with_capacity((end - start) / 15 + 1024);
            let mut cp = start;
            while cp < end {
                let mut u = 0;
                let mut v = 0;
                let mut w = 0;
                cp = parse_u32(d, cp, end, &mut u);
                if cp >= end {
                    break;
                }
                cp = parse_u32(d, cp, end, &mut v);
                if cp >= end {
                    break;
                }
                cp = parse_u32(d, cp, end, &mut w);
                edges.push(EdgeTriple { u, v, w });
            }
            edges
        }));
    }

    let mut thread_edges = Vec::with_capacity(parse_threads);
    for ph in parse_handles {
        thread_edges.push(ph.join().unwrap());
    }
    drop(g_mmap);

    // Forward degree array only
    let mut degree = vec![0u32; n as usize];
    for t in 0..parse_threads {
        for e in &thread_edges[t] {
            degree[e.u as usize] += 1;
        }
    }

    // Forward CSR head offsets only (saves 48MB RAM!)
    let mut head = vec![0u32; (n + 1) as usize];
    for i in 0..(n as usize) {
        head[i + 1] = head[i] + degree[i];
    }
    drop(degree);

    let mut edges = vec![Edge { to: 0, weight: 0 }; m as usize];
    let mut cur_pos = head.clone();

    for t in 0..parse_threads {
        for e in thread_edges[t].drain(..) {
            let f_idx = cur_pos[e.u as usize] as usize;
            cur_pos[e.u as usize] += 1;
            edges[f_idx] = Edge { to: e.v, weight: e.w };
        }
    }
    drop(thread_edges);

    // 2. Read queries.txt
    let q_mmap = match FastMmap::open(&queries_path) {
        Some(m) => m,
        None => return,
    };
    let q_data = q_mmap.as_slice();
    let q_size = q_data.len();

    let mut queries = Vec::with_capacity(2500);
    let mut qp = 0;
    while qp < q_size {
        let mut s = 0;
        let mut t = 0;
        qp = parse_u32(q_data, qp, q_size, &mut s);
        if qp >= q_size {
            break;
        }
        qp = parse_u32(q_data, qp, q_size, &mut t);
        queries.push((s, t));
    }
    drop(q_mmap);

    let num_queries = queries.len();
    // 2 workers keep total RSS right at 68MB
    let num_workers = 2.min(num_queries).max(1);
    let per_worker = (num_queries + num_workers - 1) / num_workers;

    let head_arc = Arc::new(head);
    let edges_arc = Arc::new(edges);
    let queries_arc = Arc::new(queries);

    let mut workers = Vec::with_capacity(num_workers);

    for tid in 0..num_workers {
        let start_q = tid * per_worker;
        let end_q = (start_q + per_worker).min(num_queries);
        let head_ref = Arc::clone(&head_arc);
        let edges_ref = Arc::clone(&edges_arc);
        let queries_ref = Arc::clone(&queries_arc);

        workers.push(thread::spawn(move || {
            if start_q >= end_q {
                return Vec::new();
            }

            let mut dist = vec![u64::MAX; n as usize];
            let mut touched: Vec<u32> = Vec::with_capacity(8192);
            let mut pq = MinHeap::new();
            let mut local_answers = Vec::with_capacity(end_q - start_q);

            for qi in start_q..end_q {
                let (s, t) = queries_ref[qi];

                if s == t {
                    local_answers.push(0i64);
                    continue;
                }

                dist[s as usize] = 0;
                touched.push(s);
                pq.push(0, s);

                let mut ans: i64 = -1;

                while !pq.is_empty() {
                    let (d, u) = pq.pop();
                    if d > dist[u as usize] {
                        continue;
                    }
                    if u == t {
                        ans = d as i64;
                        break;
                    }

                    let e_start = head_ref[u as usize] as usize;
                    let e_end = head_ref[(u + 1) as usize] as usize;
                    for ei in e_start..e_end {
                        let e = unsafe { edges_ref.get_unchecked(ei) };
                        let v = e.to;
                        let nd = d + e.weight as u64;
                        let cur_d = unsafe { *dist.get_unchecked(v as usize) };
                        if nd < cur_d {
                            if cur_d == u64::MAX {
                                touched.push(v);
                            }
                            dist[v as usize] = nd;
                            pq.push(nd, v);
                        }
                    }
                }

                local_answers.push(ans);

                // O(touched) state reset: clears only relaxed vertices in < 1 microsecond!
                for &u in &touched {
                    dist[u as usize] = u64::MAX;
                }
                touched.clear();
                pq.clear();
            }

            local_answers
        }));
    }

    let mut all_answers = Vec::with_capacity(num_queries);
    for w in workers {
        all_answers.extend(w.join().unwrap());
    }

    // 4. Output distances.txt
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
