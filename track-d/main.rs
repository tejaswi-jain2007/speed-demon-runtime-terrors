// Speed Demon 2026 - Track D (Traverse)
// Team: Runtime Terrors
// Ultra-fast 4-ary MinHeap Dijkstra solver in pure Rust (low-memory zero-fault distance array)

use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use std::thread;

const PROT_READ: i32 = 1;
const MAP_PRIVATE: i32 = 2;
const MADV_SEQUENTIAL: i32 = 2;

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
        if ptr == std::ptr::null_mut() || ptr as isize == -1 { return None; }
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

#[derive(Clone, Copy)]
struct Edge {
    to: u32,
    weight: u32,
}

struct MinHeap {
    data: Vec<(u64, u32)>,
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
            let mut idx = 0;
            let len = self.data.len();
            loop {
                let child = (idx << 2) + 1;
                if child >= len {
                    break;
                }
                let mut best = child;
                let end = (child + 4).min(len);
                for c in (child + 1)..end {
                    if unsafe { self.data.get_unchecked(c).0 < self.data.get_unchecked(best).0 } {
                        best = c;
                    }
                }
                if unsafe { self.data.get_unchecked(best).0 < last.0 } {
                    unsafe { *self.data.get_unchecked_mut(idx) = *self.data.get_unchecked(best); }
                    idx = best;
                } else {
                    break;
                }
            }
            self.data[idx] = last;
        }
        min
    }
}

#[inline(always)]
fn parse_u32(d: &[u8], mut p: usize, end: usize, val: &mut u32) -> usize {
    while p < end && d[p] <= b' ' { p += 1; }
    let mut v = 0u64;
    while p < end && d[p] >= b'0' && d[p] <= b'9' {
        v = v * 10 + (d[p] - b'0') as u64;
        p += 1;
    }
    *val = v as u32;
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
    gp = parse_u32(g_data, gp, g_size, &mut n);
    gp = parse_u32(g_data, gp, g_size, &mut m);
    let edges_start = gp;

    let num_words = ((n as usize) + 63) / 64;
    let mut has_in_edges = vec![0u64; num_words];
    // In-place CSR building: head uses size n + 2, eliminating degree and cur_pos vectors (saves 14MB)
    let mut head = vec![0u32; (n + 2) as usize];

    let mut p = edges_start;
    for _ in 0..m {
        let mut u = 0;
        let mut v = 0;
        let mut w = 0;
        p = parse_u32(g_data, p, g_size, &mut u);
        p = parse_u32(g_data, p, g_size, &mut v);
        p = parse_u32(g_data, p, g_size, &mut w);
        head[(u + 1) as usize] += 1;
        has_in_edges[(v as usize) >> 6] |= 1u64 << ((v as usize) & 63);
    }

    for j in 0..=n as usize {
        head[j + 1] += head[j];
    }

    let mut edges = vec![Edge { to: 0, weight: 0 }; m as usize];

    p = edges_start;
    for _ in 0..m {
        let mut u = 0;
        let mut v = 0;
        let mut w = 0;
        p = parse_u32(g_data, p, g_size, &mut u);
        p = parse_u32(g_data, p, g_size, &mut v);
        p = parse_u32(g_data, p, g_size, &mut w);
        let idx = head[u as usize];
        head[u as usize] += 1;
        edges[idx as usize] = Edge { to: v, weight: w };
    }
    drop(g_mmap);

    // Restore head offsets
    for j in (0..n as usize).rev() {
        head[j + 1] = head[j];
    }
    head[0] = 0;
    head.truncate((n + 1) as usize);

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
        if qp >= q_size { break; }
        qp = parse_u32(q_data, qp, q_size, &mut t);
        queries.push((s, t));
    }
    drop(q_mmap);

    let num_queries = queries.len();
    let num_workers = 10.min(num_queries).max(1);
    let per_worker = (num_queries + num_workers - 1) / num_workers;

    let head_arc = Arc::new(head);
    let edges_arc = Arc::new(edges);
    let has_in_arc = Arc::new(has_in_edges);
    let queries_arc = Arc::new(queries);

    let mut workers = Vec::with_capacity(num_workers);

    for tid in 0..num_workers {
        let start_q = tid * per_worker;
        let end_q = (start_q + per_worker).min(num_queries);
        let head_ref = Arc::clone(&head_arc);
        let edges_ref = Arc::clone(&edges_arc);
        let in_ref = Arc::clone(&has_in_arc);
        let queries_ref = Arc::clone(&queries_arc);

        workers.push(thread::spawn(move || {
            if start_q >= end_q { return Vec::new(); }

            // Zero-initialized distance vector: OS COW zero-pages prevent physical RSS fault
            // Stored as dist + 1 (so 0 means unvisited)
            let mut dist = vec![0u64; n as usize];
            let mut touched: Vec<u32> = Vec::with_capacity(8192);
            let mut pq = MinHeap::new();
            let mut local_answers = Vec::with_capacity(end_q - start_q);

            for qi in start_q..end_q {
                let (s, t) = queries_ref[qi];

                if s == t {
                    local_answers.push(0i64);
                    continue;
                }

                let t_has_in = (in_ref[(t as usize) >> 6] & (1u64 << ((t as usize) & 63))) != 0;
                if !t_has_in || head_ref[s as usize] == head_ref[(s + 1) as usize] {
                    local_answers.push(-1i64);
                    continue;
                }

                dist[s as usize] = 1; // 0 + 1
                touched.push(s);
                pq.push(0, s);

                let mut ans: i64 = -1;

                while !pq.is_empty() {
                    let (d, u) = pq.pop();
                    let cur_u_stored = unsafe { *dist.get_unchecked(u as usize) };
                    if cur_u_stored == 0 || d > cur_u_stored - 1 {
                        continue;
                    }
                    if u == t {
                        ans = d as i64;
                        break;
                    }

                    let start = head_ref[u as usize] as usize;
                    let end = head_ref[(u + 1) as usize] as usize;
                    for ei in start..end {
                        let e = unsafe { edges_ref.get_unchecked(ei) };
                        let nd = d + e.weight as u64;
                        let cur_stored = unsafe { *dist.get_unchecked(e.to as usize) };
                        if cur_stored == 0 || nd < cur_stored - 1 {
                            if cur_stored == 0 {
                                touched.push(e.to);
                            }
                            dist[e.to as usize] = nd + 1;
                            pq.push(nd, e.to);
                        }
                    }
                }

                local_answers.push(ans);

                for &u in &touched {
                    dist[u as usize] = 0;
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
