use std::collections::{BinaryHeap, HashMap};
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
        unsafe { madvise(ptr, len, 2); }
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
    fn new() -> Self { MinHeap { data: Vec::with_capacity(16384) } }
    #[inline(always)]
    fn clear(&mut self) { self.data.clear(); }
    #[inline(always)]
    fn is_empty(&self) -> bool { self.data.is_empty() }
    #[inline(always)]
    fn push(&mut self, dist: u32, u: u32) {
        let val = ((dist as u64) << 32) | (u as u64);
        let mut idx = self.data.len();
        self.data.push(val);
        while idx > 0 {
            let parent = (idx - 1) >> 2;
            let p_val = unsafe { *self.data.get_unchecked(parent) };
            if val < p_val {
                unsafe {
                    *self.data.get_unchecked_mut(idx) = p_val;
                    *self.data.get_unchecked_mut(parent) = val;
                }
                idx = parent;
            } else { break; }
        }
    }
    #[inline(always)]
    fn pop(&mut self) -> (u32, u32) {
        let min = self.data[0];
        let last = self.data.pop().unwrap();
        if !self.data.is_empty() {
            let len = self.data.len();
            let mut idx = 0;
            unsafe { *self.data.get_unchecked_mut(0) = last; }
            while (idx << 2) + 1 < len {
                let first_child = (idx << 2) + 1;
                let mut min_child = first_child;
                let mut min_val = unsafe { *self.data.get_unchecked(first_child) };
                let end_child = (first_child + 4).min(len);
                for c in (first_child + 1)..end_child {
                    let cv = unsafe { *self.data.get_unchecked(c) };
                    if cv < min_val { min_val = cv; min_child = c; }
                }
                if min_val < last {
                    unsafe {
                        *self.data.get_unchecked_mut(idx) = min_val;
                        *self.data.get_unchecked_mut(min_child) = last;
                    }
                    idx = min_child;
                } else { break; }
            }
        }
        ((min >> 32) as u32, min as u32)
    }
}

#[inline(always)]
fn parse_u32(d: &[u8], mut p: usize, end: usize, val: &mut u32) -> usize {
    while p < end && unsafe { *d.get_unchecked(p) } <= b' ' { p += 1; }
    let mut v = 0u32;
    while p < end && unsafe { *d.get_unchecked(p) } >= b'0' { v = v * 10 + (unsafe { *d.get_unchecked(p) } - b'0') as u32; p += 1; }
    *val = v; p
}

fn dijkstra64(head: &[u32], edges: &[Edge], s: u32, t: u32) -> i64 {
    let mut d64: HashMap<u32, u64> = HashMap::with_capacity(64);
    let mut pq: BinaryHeap<std::cmp::Reverse<(u64, u32)>> = BinaryHeap::with_capacity(64);
    d64.insert(s, 0); pq.push(std::cmp::Reverse((0, s)));
    while let Some(std::cmp::Reverse((d, u))) = pq.pop() {
        if d > *d64.get(&u).unwrap_or(&u64::MAX) { continue; }
        if u == t { return d as i64; }
        for ei in (head[u as usize] as usize)..(head[(u + 1) as usize] as usize) {
            let nd = d + edges[ei].weight as u64;
            if nd < *d64.get(&edges[ei].to).unwrap_or(&u64::MAX) {
                d64.insert(edges[ei].to, nd); pq.push(std::cmp::Reverse((nd, edges[ei].to)));
            }
        }
    }
    -1
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

    let mut head = vec![0u32; (n + 2) as usize];
    let mut p = edges_start;
    for _ in 0..m {
        while unsafe { *g_data.get_unchecked(p) } <= b' ' { p += 1; }
        let mut u = 0u32;
        while unsafe { *g_data.get_unchecked(p) } >= b'0' { u = u * 10 + (unsafe { *g_data.get_unchecked(p) } - b'0') as u32; p += 1; }
        while unsafe { *g_data.get_unchecked(p) } <= b' ' { p += 1; }
        while unsafe { *g_data.get_unchecked(p) } >= b'0' { p += 1; }
        while unsafe { *g_data.get_unchecked(p) } <= b' ' { p += 1; }
        while unsafe { *g_data.get_unchecked(p) } >= b'0' { p += 1; }
        head[(u + 1) as usize] += 1;
    }
    for j in 0..=n as usize { head[j + 1] += head[j]; }

    let mut edges = vec![Edge { to: 0, weight: 0 }; m as usize];
    p = edges_start;
    for _ in 0..m {
        while unsafe { *g_data.get_unchecked(p) } <= b' ' { p += 1; }
        let (mut u, mut v, mut w) = (0u32, 0u32, 0u32);
        while unsafe { *g_data.get_unchecked(p) } >= b'0' { u = u * 10 + (unsafe { *g_data.get_unchecked(p) } - b'0') as u32; p += 1; }
        while unsafe { *g_data.get_unchecked(p) } <= b' ' { p += 1; }
        while unsafe { *g_data.get_unchecked(p) } >= b'0' { v = v * 10 + (unsafe { *g_data.get_unchecked(p) } - b'0') as u32; p += 1; }
        while unsafe { *g_data.get_unchecked(p) } <= b' ' { p += 1; }
        while unsafe { *g_data.get_unchecked(p) } >= b'0' { w = w * 10 + (unsafe { *g_data.get_unchecked(p) } - b'0') as u32; p += 1; }
        let idx = head[u as usize];
        head[u as usize] += 1;
        edges[idx as usize] = Edge { to: v, weight: w };
    }
    drop(g_mmap);
    for j in (0..n as usize).rev() { head[j + 1] = head[j]; }
    head[0] = 0; head.truncate((n + 1) as usize);

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
    let num_workers = 6.min(num_queries).max(1);
    let (head_arc, edges_arc, queries_arc, next_q_arc) =
        (Arc::new(head), Arc::new(edges), Arc::new(queries), Arc::new(AtomicUsize::new(0)));
    let mut all_answers = vec![0i64; num_queries];
    let answers_ptr = all_answers.as_mut_ptr() as usize;

    let mut workers = Vec::with_capacity(num_workers);
    for _ in 0..num_workers {
        let (head_ref, edges_ref, queries_ref, next_q) =
            (Arc::clone(&head_arc), Arc::clone(&edges_arc), Arc::clone(&queries_arc), Arc::clone(&next_q_arc));
        let builder = thread::Builder::new().stack_size(256 * 1024);
        workers.push(builder.spawn(move || {
            let out_ptr = answers_ptr as *mut i64;
            let mut dist16 = vec![0u16; n as usize];
            let mut touched: Vec<u32> = Vec::with_capacity(16384);
            let mut pq = MinHeap::new();
            loop {
                let qi = next_q.fetch_add(1, Ordering::Relaxed);
                if qi >= num_queries { break; }
                let (s, t) = queries_ref[qi];
                if s == t { unsafe { *out_ptr.add(qi) = 0; } continue; }
                let (start, end) = (*unsafe { head_ref.get_unchecked(s as usize) } as usize, *unsafe { head_ref.get_unchecked((s + 1) as usize) } as usize);
                if start == end { unsafe { *out_ptr.add(qi) = -1; } continue; }
                unsafe { *dist16.get_unchecked_mut(s as usize) = 1; }
                touched.push(s); pq.push(0, s);
                let (mut best_t, mut overflow) = (u32::MAX, false);
                while !pq.is_empty() {
                    let (d, u) = pq.pop();
                    if d >= best_t { break; }
                    let cur_u_stored = unsafe { *dist16.get_unchecked(u as usize) };
                    if (d + 1) as u16 > cur_u_stored { continue; }
                    if u == t { best_t = d; break; }
                    let (u_start, u_end) = (*unsafe { head_ref.get_unchecked(u as usize) } as usize, *unsafe { head_ref.get_unchecked((u + 1) as usize) } as usize);
                    for ei in u_start..u_end {
                        let e = unsafe { edges_ref.get_unchecked(ei) };
                        let nd = d + e.weight;
                        if nd >= 65000 { overflow = true; break; }
                        if nd >= best_t { continue; }
                        if e.to == t { best_t = nd; continue; }
                        let cur_stored = unsafe { *dist16.get_unchecked(e.to as usize) };
                        let new_stored = (nd + 1) as u16;
                        if cur_stored == 0 || new_stored < cur_stored {
                            if cur_stored == 0 { touched.push(e.to); }
                            unsafe { *dist16.get_unchecked_mut(e.to as usize) = new_stored; }
                            pq.push(nd, e.to);
                        }
                    }
                    if overflow { break; }
                }
                for &u in &touched { unsafe { *dist16.get_unchecked_mut(u as usize) = 0; } }
                touched.clear(); pq.clear();
                let ans = if overflow { dijkstra64(&head_ref, &edges_ref, s, t) } else if best_t == u32::MAX { -1 } else { best_t as i64 };
                unsafe { *out_ptr.add(qi) = ans; }
            }
        }).unwrap());
    }
    for w in workers { w.join().unwrap(); }

    let mut out_buffer = Vec::with_capacity(32 * 1024);
    for d in all_answers {
        if d < 0 { out_buffer.extend_from_slice(b"-1\n"); }
        else if d == 0 { out_buffer.extend_from_slice(b"0\n"); }
        else {
            let (mut temp, mut idx, mut v) = ([0u8; 20], 20, d as u64);
            while v > 0 { idx -= 1; temp[idx] = b'0' + (v % 10) as u8; v /= 10; }
            out_buffer.extend_from_slice(&temp[idx..]); out_buffer.push(b'\n');
        }
    }
    let _ = std::fs::create_dir_all(&out_dir);
    if let Ok(mut f) = File::create(&out_path) { let _ = f.write_all(&out_buffer); }
}
