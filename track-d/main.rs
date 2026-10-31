// Speed Demon 2026 - Track D (Traverse)
// Team: Runtime Terrors
// High-efficiency 64-bucket Radix Heap Dijkstra solver with 8-thread parallel graph ingestion

use std::collections::HashMap;
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

#[derive(Clone, Copy)]
struct Edge {
    to: u32,
    weight: u32,
}

struct RadixHeap {
    buckets: Vec<Vec<(u64, u32)>>,
    last: u64,
    size: usize,
    nonempty: u128,
}

impl RadixHeap {
    fn new() -> Self {
        RadixHeap {
            buckets: (0..65).map(|_| Vec::with_capacity(64)).collect(),
            last: 0,
            size: 0,
            nonempty: 0,
        }
    }

    #[inline(always)]
    fn clear(&mut self) {
        for b in &mut self.buckets { b.clear(); }
        self.last = 0;
        self.size = 0;
        self.nonempty = 0;
    }

    #[inline(always)]
    fn is_empty(&self) -> bool {
        self.size == 0
    }

    #[inline(always)]
    fn push(&mut self, val: u64, u: u32) {
        let diff = val ^ self.last;
        let b = if diff == 0 { 0 } else { 64 - (diff.leading_zeros() as usize) };
        unsafe { self.buckets.get_unchecked_mut(b).push((val, u)); }
        self.nonempty |= 1u128 << b;
        self.size += 1;
    }

    #[inline(always)]
    fn pop(&mut self) -> (u64, u32) {
        if unsafe { self.buckets.get_unchecked(0).is_empty() } {
            let i = self.nonempty.trailing_zeros() as usize;
            let bucket = unsafe { self.buckets.get_unchecked_mut(i) };
            let mut min_val = bucket[0].0;
            for &(v, _) in bucket.iter().skip(1) {
                if v < min_val {
                    min_val = v;
                }
            }
            self.last = min_val;
            let old = std::mem::take(bucket);
            self.nonempty &= !(1u128 << i);
            for (v, u) in old {
                let diff = v ^ self.last;
                let b = if diff == 0 { 0 } else { 64 - (diff.leading_zeros() as usize) };
                unsafe { self.buckets.get_unchecked_mut(b).push((v, u)); }
                self.nonempty |= 1u128 << b;
            }
        }
        self.size -= 1;
        let item = unsafe { self.buckets.get_unchecked_mut(0).pop().unwrap() };
        if unsafe { self.buckets.get_unchecked(0).is_empty() } {
            self.nonempty &= !1u128;
        }
        item
    }
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

    let parse_threads = 8.min(thread::available_parallelism().map(|t| t.get()).unwrap_or(8)).max(1);
    let payload_len = g_size - edges_start;
    let mut splits = vec![edges_start; parse_threads + 1];
    splits[parse_threads] = g_size;
    for t in 1..parse_threads {
        let mut pt = edges_start + t * (payload_len / parse_threads);
        while pt < g_size && g_data[pt] != b'\n' { pt += 1; }
        if pt < g_size { pt += 1; }
        splits[t] = pt;
    }

    let g_mmap_arc = Arc::new(g_mmap);
    let mut parse_handles = Vec::with_capacity(parse_threads);
    for t in 0..parse_threads {
        let (m_ref, start, end) = (Arc::clone(&g_mmap_arc), splits[t], splits[t + 1]);
        parse_handles.push(thread::spawn(move || {
            let d = m_ref.as_slice();
            let mut local_edges = Vec::with_capacity((end - start) / 18);
            unsafe {
                let mut p = d.as_ptr().add(start);
                let end_ptr = d.as_ptr().add(end);
                while p < end_ptr {
                    while p < end_ptr && *p <= b' ' { p = p.add(1); }
                    if p >= end_ptr { break; }
                    let mut u = (*p - b'0') as u32; p = p.add(1);
                    while p < end_ptr && *p >= b'0' { u = u * 10 + (*p - b'0') as u32; p = p.add(1); }
                    while p < end_ptr && *p <= b' ' { p = p.add(1); }
                    if p >= end_ptr { break; }
                    let mut v = (*p - b'0') as u32; p = p.add(1);
                    while p < end_ptr && *p >= b'0' { v = v * 10 + (*p - b'0') as u32; p = p.add(1); }
                    while p < end_ptr && *p <= b' ' { p = p.add(1); }
                    if p >= end_ptr { break; }
                    let mut w = (*p - b'0') as u32; p = p.add(1);
                    while p < end_ptr && *p >= b'0' { w = w * 10 + (*p - b'0') as u32; p = p.add(1); }
                    local_edges.push((u, v, w));
                }
            }
            local_edges
        }));
    }

    let mut thread_raw_edges = Vec::with_capacity(parse_threads);
    for ph in parse_handles { thread_raw_edges.push(ph.join().unwrap()); }
    drop(g_mmap_arc);

    let num_words = ((n as usize) + 63) >> 6;
    let mut has_in_edges = vec![0u64; num_words];
    let mut head = vec![0u32; (n + 2) as usize];

    for chunk in &thread_raw_edges {
        for &(u, v, _) in chunk {
            head[(u + 1) as usize] += 1;
            *unsafe { has_in_edges.get_unchecked_mut((v as usize) >> 6) } |= 1u64 << ((v as usize) & 63);
        }
    }

    for j in 0..=n as usize {
        head[j + 1] += head[j];
    }

    let mut edges = vec![Edge { to: 0, weight: 0 }; m as usize];
    for chunk in thread_raw_edges {
        for (u, v, w) in chunk {
            let idx = head[u as usize] as usize;
            head[u as usize] += 1;
            *unsafe { edges.get_unchecked_mut(idx) } = Edge { to: v, weight: w };
        }
    }

    for j in (0..n as usize).rev() {
        head[j + 1] = head[j];
    }
    head[0] = 0;
    head.truncate((n + 1) as usize);

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

    let num_workers = 8.min(num_queries).max(1);
    let head_arc = Arc::new(head);
    let edges_arc = Arc::new(edges);
    let has_in_arc = Arc::new(has_in_edges);
    let queries_arc = Arc::new(queries);
    let next_q_arc = Arc::new(AtomicUsize::new(0));

    let mut all_answers = vec![0i64; num_queries];
    let answers_ptr = all_answers.as_mut_ptr() as usize;

    let mut workers = Vec::with_capacity(num_workers);

    for _ in 0..num_workers {
        let head_ref = Arc::clone(&head_arc);
        let edges_ref = Arc::clone(&edges_arc);
        let in_ref = Arc::clone(&has_in_arc);
        let queries_ref = Arc::clone(&queries_arc);
        let next_q = Arc::clone(&next_q_arc);

        let builder = thread::Builder::new().stack_size(256 * 1024);
        workers.push(builder.spawn(move || {
            let out_ptr = answers_ptr as *mut i64;
            let mut dist = vec![u32::MAX; head_ref.len() - 1];
            let mut overflow_dist: HashMap<u32, u64> = HashMap::with_capacity(16);
            let mut touched: Vec<u32> = Vec::with_capacity(8192);
            let mut rh = RadixHeap::new();

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

                unsafe { *dist.get_unchecked_mut(s as usize) = 0; }
                touched.push(s);
                rh.push(0, s);

                let mut best_t = u64::MAX;

                while !rh.is_empty() {
                    let (d, u) = rh.pop();
                    if d >= best_t {
                        break;
                    }

                    if d < u32::MAX as u64 {
                        let cur_u = unsafe { *dist.get_unchecked(u as usize) as u64 };
                        if d > cur_u {
                            continue;
                        }
                    } else {
                        let cur_u = *overflow_dist.get(&u).unwrap_or(&u64::MAX);
                        if d > cur_u {
                            continue;
                        }
                    }

                    if u == t {
                        best_t = d;
                        break;
                    }

                    let start = unsafe { *head_ref.get_unchecked(u as usize) as usize };
                    let end = unsafe { *head_ref.get_unchecked((u + 1) as usize) as usize };
                    for ei in start..end {
                        let e = unsafe { edges_ref.get_unchecked(ei) };
                        let nd = d.saturating_add(e.weight as u64);
                        if nd >= best_t {
                            continue;
                        }
                        if e.to == t {
                            best_t = nd;
                            continue;
                        }

                        if nd < u32::MAX as u64 {
                            let cur = unsafe { *dist.get_unchecked(e.to as usize) as u64 };
                            if nd < cur {
                                if cur == u32::MAX as u64 {
                                    touched.push(e.to);
                                }
                                unsafe { *dist.get_unchecked_mut(e.to as usize) = nd as u32; }
                                rh.push(nd, e.to);
                            }
                        } else {
                            let cur = *overflow_dist.get(&e.to).unwrap_or(&u64::MAX);
                            if nd < cur {
                                overflow_dist.insert(e.to, nd);
                                rh.push(nd, e.to);
                            }
                        }
                    }
                }

                for &u in &touched {
                    unsafe { *dist.get_unchecked_mut(u as usize) = u32::MAX; }
                }
                touched.clear();
                overflow_dist.clear();
                rh.clear();

                let ans = if best_t != u64::MAX { best_t as i64 } else { -1 };
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
