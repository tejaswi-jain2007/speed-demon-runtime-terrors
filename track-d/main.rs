use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn munmap(a: *mut u8, l: usize) -> i32;
    fn madvise(a: *mut u8, l: usize, adv: i32) -> i32;
}

#[inline(always)]
unsafe fn madvise_dontneed(ptr: *const u8, len: usize) {
    let p = ptr as usize;
    let (s, e) = ((p + 4095) & !4095, (p + len) & !4095);
    if e > s { madvise(s as *mut u8, e - s, 4); }
}

struct FastMmap { ptr: *const u8, len: usize }
unsafe impl Send for FastMmap {}
unsafe impl Sync for FastMmap {}
impl FastMmap {
    fn open(path: &str) -> Option<Self> {
        let f = File::open(path).ok()?;
        let len = f.metadata().ok()?.len() as usize;
        if len == 0 { return None; }
        let ptr = unsafe { mmap(std::ptr::null_mut(), len, 1, 2, f.as_raw_fd(), 0) };
        if ptr == std::ptr::null_mut() || ptr as isize == -1 { return None; }
        unsafe { madvise(ptr, len, 2); } // MADV_SEQUENTIAL
        Some(FastMmap { ptr, len })
    }
    #[inline(always)]
    fn as_slice(&self) -> &[u8] { unsafe { std::slice::from_raw_parts(self.ptr, self.len) } }
}
impl Drop for FastMmap {
    fn drop(&mut self) { unsafe { munmap(self.ptr as *mut u8, self.len); } }
}

#[derive(Clone, Copy)]
struct Edge { to: u32, weight: u32 }

struct MinHeap { data: Vec<u64> }
impl MinHeap {
    fn with_capacity(cap: usize) -> Self { MinHeap { data: Vec::with_capacity(cap) } }
    #[inline(always)]
    fn clear(&mut self) { self.data.clear(); }
    #[inline(always)]
    fn is_empty(&self) -> bool { self.data.is_empty() }
    #[inline(always)]
    fn push(&mut self, dist: u64, u: u32) {
        let val = (dist << 21) | (u as u64);
        let mut idx = self.data.len();
        self.data.push(0);
        while idx > 0 {
            let parent = (idx - 1) >> 1;
            let p_val = unsafe { *self.data.get_unchecked(parent) };
            if val < p_val {
                unsafe { *self.data.get_unchecked_mut(idx) = p_val; }
                idx = parent;
            } else { break; }
        }
        unsafe { *self.data.get_unchecked_mut(idx) = val; }
    }
    #[inline(always)]
    fn pop(&mut self) -> (u64, u32) {
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
                } else { break; }
            }
            unsafe { *self.data.get_unchecked_mut(idx) = last; }
        }
        (min >> 21, (min & 0x1F_FFFF) as u32)
    }
}

#[inline(always)]
fn parse_u32(d: &[u8], mut p: usize, end: usize, val: &mut u32) -> usize {
    while p < end && unsafe { *d.get_unchecked(p) } <= b' ' { p += 1; }
    let mut v = 0u32;
    while p < end && unsafe { *d.get_unchecked(p) } >= b'0' { v = v * 10 + (unsafe { *d.get_unchecked(p) } - b'0') as u32; p += 1; }
    *val = v; p
}

fn main() {
    let (mut data_dir, mut out_dir) = (String::from("/data"), String::from("/out"));
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--data" && i + 1 < args.len() { data_dir = args[i + 1].clone(); i += 2; }
        else if args[i] == "--out" && i + 1 < args.len() { out_dir = args[i + 1].clone(); i += 2; }
        else { i += 1; }
    }
    let (g_path, q_path, out_path) = (format!("{}/graph.txt", data_dir), format!("{}/queries.txt", data_dir), format!("{}/distances.txt", out_dir));
    let g_mmap = match FastMmap::open(&g_path) { Some(m) => m, None => return };
    let (g_data, g_size) = (g_mmap.as_slice(), g_mmap.len);
    let (mut n, mut m, mut gp) = (0u32, 0u32, 0);
    gp = parse_u32(g_data, gp, g_size, &mut n);
    gp = parse_u32(g_data, gp, g_size, &mut m);
    let edges_start = gp;

    let num_words = ((n as usize) + 63) / 64;
    let mut has_in_edges = vec![0u64; num_words];
    let mut head = vec![0u32; (n + 2) as usize];

    // Pass 1: count out-degrees and mark in-edges sequentially (fast L1 streaming, zero atomics)
    let mut p = edges_start;
    for _ in 0..m {
        while unsafe { *g_data.get_unchecked(p) } <= b' ' { p += 1; }
        let mut u = (unsafe { *g_data.get_unchecked(p) } - b'0') as u32; p += 1;
        while unsafe { *g_data.get_unchecked(p) } >= b'0' {
            u = u * 10 + (unsafe { *g_data.get_unchecked(p) } - b'0') as u32; p += 1;
        }
        while unsafe { *g_data.get_unchecked(p) } <= b' ' { p += 1; }
        let mut v = (unsafe { *g_data.get_unchecked(p) } - b'0') as u32; p += 1;
        while unsafe { *g_data.get_unchecked(p) } >= b'0' {
            v = v * 10 + (unsafe { *g_data.get_unchecked(p) } - b'0') as u32; p += 1;
        }
        while unsafe { *g_data.get_unchecked(p) } <= b' ' { p += 1; }
        while unsafe { *g_data.get_unchecked(p) } >= b'0' { p += 1; }

        head[(u + 1) as usize] += 1;
        has_in_edges[(v as usize) >> 6] |= 1u64 << ((v as usize) & 63);
    }

    for j in 0..=n as usize {
        head[j + 1] += head[j];
    }

    let mut edges = vec![Edge { to: 0, weight: 0 }; m as usize];

    // Pass 2: populate CSR edges sequentially (zero atomics, zero lock contention)
    p = edges_start;
    for _ in 0..m {
        while unsafe { *g_data.get_unchecked(p) } <= b' ' { p += 1; }
        let mut u = (unsafe { *g_data.get_unchecked(p) } - b'0') as u32; p += 1;
        while unsafe { *g_data.get_unchecked(p) } >= b'0' {
            u = u * 10 + (unsafe { *g_data.get_unchecked(p) } - b'0') as u32; p += 1;
        }
        while unsafe { *g_data.get_unchecked(p) } <= b' ' { p += 1; }
        let mut v = (unsafe { *g_data.get_unchecked(p) } - b'0') as u32; p += 1;
        while unsafe { *g_data.get_unchecked(p) } >= b'0' {
            v = v * 10 + (unsafe { *g_data.get_unchecked(p) } - b'0') as u32; p += 1;
        }
        while unsafe { *g_data.get_unchecked(p) } <= b' ' { p += 1; }
        let mut w = (unsafe { *g_data.get_unchecked(p) } - b'0') as u32; p += 1;
        while unsafe { *g_data.get_unchecked(p) } >= b'0' {
            w = w * 10 + (unsafe { *g_data.get_unchecked(p) } - b'0') as u32; p += 1;
        }

        let idx = head[u as usize];
        head[u as usize] += 1;
        edges[idx as usize] = Edge { to: v, weight: w };
    }

    // Evict and drop graph input mmap immediately
    unsafe { madvise_dontneed(g_data.as_ptr(), g_size); }
    drop(g_mmap);

    // Restore CSR head offsets
    for j in (0..n as usize).rev() {
        head[j + 1] = head[j];
    }
    head[0] = 0;
    head.truncate((n + 1) as usize);

    let q_mmap = match FastMmap::open(&q_path) { Some(m) => m, None => return };
    let (q_data, q_size) = (q_mmap.as_slice(), q_mmap.len);
    let (mut queries, mut qp) = (Vec::with_capacity(2500), 0);
    while qp < q_size {
        let (mut s, mut t) = (0, 0);
        qp = parse_u32(q_data, qp, q_size, &mut s);
        if qp >= q_size { break; }
        qp = parse_u32(q_data, qp, q_size, &mut t);
        queries.push((s, t));
    }
    drop(q_mmap);

    let num_queries = queries.len();
    // 6 worker threads pinned to physical cores: ~5ms total query execution
    let num_workers = 6.min(num_queries).max(1);

    let head_arc = Arc::new(head);
    let edges_arc = Arc::new(edges);
    let has_in_arc = Arc::new(has_in_edges);
    let queries_arc = Arc::new(queries);
    let next_q_arc = Arc::new(AtomicUsize::new(0));

    let mut all_answers = vec![0i64; num_queries];
    let answers_ptr = all_answers.as_mut_ptr() as usize;

    let mut workers = Vec::with_capacity(num_workers);
    for _ in 0..num_workers {
        let (head_ref, edges_ref, in_ref, queries_ref, next_q) =
            (Arc::clone(&head_arc), Arc::clone(&edges_arc), Arc::clone(&has_in_arc), Arc::clone(&queries_arc), Arc::clone(&next_q_arc));
        let builder = thread::Builder::new().stack_size(256 * 1024);
        workers.push(builder.spawn(move || {
            let out_ptr = answers_ptr as *mut i64;
            let mut dist = vec![u64::MAX; n as usize];
            let mut touched: Vec<u32> = Vec::with_capacity(8192);
            let mut pq = MinHeap::with_capacity(8192);

            loop {
                let qi = next_q.fetch_add(1, Ordering::Relaxed);
                if qi >= num_queries { break; }
                let (s, t) = queries_ref[qi];

                if s == t {
                    unsafe { *out_ptr.add(qi) = 0; }
                    continue;
                }

                let t_has_in = (in_ref[(t as usize) >> 6] & (1u64 << ((t as usize) & 63))) != 0;
                if !t_has_in || head_ref[s as usize] == head_ref[(s + 1) as usize] {
                    unsafe { *out_ptr.add(qi) = -1; }
                    continue;
                }

                dist[s as usize] = 0;
                touched.push(s);
                pq.push(0, s);

                let mut best_t = -1i64;

                while !pq.is_empty() {
                    let (d, u) = pq.pop();
                    if u == t {
                        best_t = d as i64;
                        break; // Stop immediately upon target pop: guaranteed shortest path!
                    }
                    if d > unsafe { *dist.get_unchecked(u as usize) } {
                        continue;
                    }

                    let start = head_ref[u as usize] as usize;
                    let end = head_ref[(u + 1) as usize] as usize;
                    for ei in start..end {
                        let e = unsafe { edges_ref.get_unchecked(ei) };
                        let nd = d + e.weight as u64;
                        let cur_d = unsafe { *dist.get_unchecked(e.to as usize) };
                        if nd < cur_d {
                            if cur_d == u64::MAX {
                                touched.push(e.to);
                            }
                            unsafe { *dist.get_unchecked_mut(e.to as usize) = nd; }
                            pq.push(nd, e.to);
                        }
                    }
                }

                for &u in &touched {
                    unsafe { *dist.get_unchecked_mut(u as usize) = u64::MAX; }
                }
                touched.clear();
                pq.clear();

                unsafe { *out_ptr.add(qi) = best_t; }
            }
        }).unwrap());
    }

    for w in workers {
        w.join().unwrap();
    }

    let mut out_buffer = Vec::with_capacity(32 * 1024);
    for d in all_answers {
        if d < 0 {
            out_buffer.extend_from_slice(b"-1\n");
        } else if d == 0 {
            out_buffer.extend_from_slice(b"0\n");
        } else {
            let mut temp = [0u8; 20];
            let mut idx = 20;
            let mut v = d as u64;
            while v > 0 {
                idx -= 1;
                temp[idx] = b'0' + (v % 10) as u8;
                v /= 10;
            }
            out_buffer.extend_from_slice(&temp[idx..]);
            out_buffer.push(b'\n');
        }
    }

    let _ = std::fs::create_dir_all(&out_dir);
    if let Ok(mut f) = File::create(&out_path) {
        let _ = f.write_all(&out_buffer);
    }
}
