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

struct BinaryMinHeap { data: Vec<u64> }
impl BinaryMinHeap {
    fn with_capacity(cap: usize) -> Self { BinaryMinHeap { data: Vec::with_capacity(cap) } }
    #[inline(always)]
    fn clear(&mut self) { self.data.clear(); }
    #[inline(always)]
    fn is_empty(&self) -> bool { self.data.is_empty() }
    #[inline(always)]
    fn push(&mut self, dist: u64, u: u32) {
        let val = (dist << 21) | (u as u64);
        let mut idx = self.data.len();
        self.data.push(val);
        while idx > 0 {
            let parent = (idx - 1) >> 1;
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
    fn pop(&mut self) -> (u64, u32) {
        let min = unsafe { *self.data.get_unchecked(0) };
        let last = self.data.pop().unwrap();
        let len = self.data.len();
        if len > 0 {
            unsafe { *self.data.get_unchecked_mut(0) = last; }
            let mut idx = 0;
            while (idx << 1) + 1 < len {
                let left = (idx << 1) + 1;
                let right = left + 1;
                let mut min_child = left;
                let mut min_val = unsafe { *self.data.get_unchecked(left) };
                if right < len {
                    let r_val = unsafe { *self.data.get_unchecked(right) };
                    if r_val < min_val { min_val = r_val; min_child = right; }
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
    let g_mmap = match FastMmap::open(&g_path) { Some(m) => Arc::new(m), None => return };
    let (g_data, g_size) = (g_mmap.as_slice(), g_mmap.len);
    let (mut n, mut m, mut gp) = (0u32, 0u32, 0);
    gp = parse_u32(g_data, gp, g_size, &mut n);
    gp = parse_u32(g_data, gp, g_size, &mut m);
    let edges_start = gp;

    let num_parse_threads = 4;
    let total_edge_bytes = g_size - edges_start;
    let mut splits = vec![edges_start; num_parse_threads + 1];
    splits[num_parse_threads] = g_size;
    for t in 1..num_parse_threads {
        let mut pt = edges_start + t * (total_edge_bytes / num_parse_threads);
        while pt < g_size && g_data[pt] != b'\n' { pt += 1; }
        if pt < g_size { pt += 1; }
        splits[t] = pt.min(g_size);
    }

    let has_in_words = (n as usize + 63) / 64;

    // Pass 1: 4 threads, ZERO ATOMICS. Each counts out-degree and in-edges locally in L1/L2 cache.
    let mut p1_handles = Vec::with_capacity(num_parse_threads);
    for t in 0..num_parse_threads {
        let (mmap_ref, start, end) = (Arc::clone(&g_mmap), splits[t], splits[t + 1]);
        p1_handles.push(thread::spawn(move || {
            let d = mmap_ref.as_slice();
            let mut local_counts = vec![0u16; (n + 1) as usize];
            let mut local_in = vec![0u64; has_in_words];
            let mut p = start;
            while p < end {
                while p < end && unsafe { *d.get_unchecked(p) } <= b' ' { p += 1; }
                if p >= end { break; }
                let mut u = (unsafe { *d.get_unchecked(p) } - b'0') as u32; p += 1;
                while p < end && unsafe { *d.get_unchecked(p) } >= b'0' {
                    u = u * 10 + (unsafe { *d.get_unchecked(p) } - b'0') as u32; p += 1;
                }
                while p < end && unsafe { *d.get_unchecked(p) } <= b' ' { p += 1; }
                let mut v = (unsafe { *d.get_unchecked(p) } - b'0') as u32; p += 1;
                while p < end && unsafe { *d.get_unchecked(p) } >= b'0' {
                    v = v * 10 + (unsafe { *d.get_unchecked(p) } - b'0') as u32; p += 1;
                }
                while p < end && unsafe { *d.get_unchecked(p) } <= b' ' { p += 1; }
                while p < end && unsafe { *d.get_unchecked(p) } >= b'0' { p += 1; }

                unsafe {
                    *local_counts.get_unchecked_mut(u as usize) += 1;
                    *local_in.get_unchecked_mut((v >> 6) as usize) |= 1u64 << (v & 63);
                }
            }
            (local_counts, local_in)
        }));
    }

    let mut thread_counts = Vec::with_capacity(num_parse_threads);
    let mut has_in = vec![0u64; has_in_words];
    for h in p1_handles {
        let (counts, in_bits) = h.join().unwrap();
        for (w, &b) in in_bits.iter().enumerate() { has_in[w] |= b; }
        thread_counts.push(counts);
    }

    // Prefix sum across the 4 thread count arrays to compute:
    // 1) CSR `head`
    // 2) Thread-specific private start offsets for Pass 2 (zero atomics)
    let mut head = vec![0u32; (n + 2) as usize];
    let mut thread_offsets: Vec<Vec<u32>> = (0..num_parse_threads)
        .map(|_| vec![0u32; (n + 1) as usize])
        .collect();

    let mut running_sum = 0u32;
    for u in 0..=n as usize {
        head[u] = running_sum;
        let c0 = thread_counts[0][u] as u32;
        let c1 = thread_counts[1][u] as u32;
        let c2 = thread_counts[2][u] as u32;
        let c3 = thread_counts[3][u] as u32;

        thread_offsets[0][u] = running_sum;
        thread_offsets[1][u] = running_sum + c0;
        thread_offsets[2][u] = running_sum + c0 + c1;
        thread_offsets[3][u] = running_sum + c0 + c1 + c2;

        running_sum += c0 + c1 + c2 + c3;
    }
    head[(n + 1) as usize] = running_sum;
    drop(thread_counts);

    // Pass 2: 4 threads populate CSR edges in parallel with ZERO ATOMICS!
    let mut edges = vec![Edge { to: 0, weight: 0 }; m as usize];
    let edges_ptr = edges.as_mut_ptr() as usize;

    let mut p2_handles = Vec::with_capacity(num_parse_threads);
    for (t, mut offsets) in thread_offsets.into_iter().enumerate() {
        let (mmap_ref, start, end) = (Arc::clone(&g_mmap), splits[t], splits[t + 1]);
        p2_handles.push(thread::spawn(move || {
            let d = mmap_ref.as_slice();
            let e_ptr = edges_ptr as *mut Edge;
            let mut p = start;
            while p < end {
                while p < end && unsafe { *d.get_unchecked(p) } <= b' ' { p += 1; }
                if p >= end { break; }
                let mut u = (unsafe { *d.get_unchecked(p) } - b'0') as u32; p += 1;
                while p < end && unsafe { *d.get_unchecked(p) } >= b'0' {
                    u = u * 10 + (unsafe { *d.get_unchecked(p) } - b'0') as u32; p += 1;
                }
                while p < end && unsafe { *d.get_unchecked(p) } <= b' ' { p += 1; }
                let mut v = (unsafe { *d.get_unchecked(p) } - b'0') as u32; p += 1;
                while p < end && unsafe { *d.get_unchecked(p) } >= b'0' {
                    v = v * 10 + (unsafe { *d.get_unchecked(p) } - b'0') as u32; p += 1;
                }
                while p < end && unsafe { *d.get_unchecked(p) } <= b' ' { p += 1; }
                let mut w = (unsafe { *d.get_unchecked(p) } - b'0') as u32; p += 1;
                while p < end && unsafe { *d.get_unchecked(p) } >= b'0' {
                    w = w * 10 + (unsafe { *d.get_unchecked(p) } - b'0') as u32; p += 1;
                }

                let idx = unsafe {
                    let off_ref = offsets.get_unchecked_mut(u as usize);
                    let cur = *off_ref;
                    *off_ref += 1;
                    cur
                };
                unsafe { *e_ptr.add(idx as usize) = Edge { to: v, weight: w }; }
            }
        }));
    }
    for h in p2_handles { h.join().unwrap(); }

    // Evict and drop graph input mmap immediately to minimize RSS
    unsafe { madvise_dontneed(g_data.as_ptr(), g_size); }
    drop(g_mmap);
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
    let num_workers = 4.min(num_queries).max(1);
    let (head_arc, edges_arc, has_in_arc, queries_arc, next_q_arc) =
        (Arc::new(head), Arc::new(edges), Arc::new(has_in), Arc::new(queries), Arc::new(AtomicUsize::new(0)));
    let mut all_answers = vec![0i64; num_queries];
    let answers_ptr = all_answers.as_mut_ptr() as usize;

    let mut workers = Vec::with_capacity(num_workers);
    for _ in 0..num_workers {
        let (head_ref, edges_ref, has_in_ref, queries_ref, next_q) =
            (Arc::clone(&head_arc), Arc::clone(&edges_arc), Arc::clone(&has_in_arc), Arc::clone(&queries_arc), Arc::clone(&next_q_arc));
        let builder = thread::Builder::new().stack_size(256 * 1024);
        workers.push(builder.spawn(move || {
            let out_ptr = answers_ptr as *mut i64;
            let mut dist = vec![u64::MAX; n as usize];
            let mut touched: Vec<u32> = Vec::with_capacity(8192);
            let mut pq = BinaryMinHeap::with_capacity(8192);

            loop {
                let qi = next_q.fetch_add(1, Ordering::Relaxed);
                if qi >= num_queries { break; }
                let (s, t) = queries_ref[qi];
                if s == t { unsafe { *out_ptr.add(qi) = 0; } continue; }

                let s_start = *unsafe { head_ref.get_unchecked(s as usize) } as usize;
                let s_end = *unsafe { head_ref.get_unchecked((s + 1) as usize) } as usize;
                if s_start == s_end { unsafe { *out_ptr.add(qi) = -1; } continue; }

                let t_has_in = unsafe { (*has_in_ref.get_unchecked((t >> 6) as usize) & (1 << (t & 63))) != 0 };
                if !t_has_in { unsafe { *out_ptr.add(qi) = -1; } continue; }

                let mut best_t = u64::MAX;
                for ei in s_start..s_end {
                    let e = unsafe { edges_ref.get_unchecked(ei) };
                    if e.to == t && (e.weight as u64) < best_t { best_t = e.weight as u64; }
                }

                unsafe { *dist.get_unchecked_mut(s as usize) = 0; }
                touched.push(s);
                pq.push(0, s);

                while !pq.is_empty() {
                    let (d, u) = pq.pop();
                    if d >= best_t { break; }
                    let cur_dist = unsafe { *dist.get_unchecked(u as usize) };
                    if d > cur_dist { continue; }
                    if u == t { best_t = d; break; }

                    let u_start = *unsafe { head_ref.get_unchecked(u as usize) } as usize;
                    let u_end = *unsafe { head_ref.get_unchecked((u + 1) as usize) } as usize;
                    for ei in u_start..u_end {
                        let e = unsafe { edges_ref.get_unchecked(ei) };
                        let nd = d + e.weight as u64;
                        if nd >= best_t { continue; }
                        if e.to == t { best_t = nd; continue; }
                        let cur_to = unsafe { *dist.get_unchecked(e.to as usize) };
                        if nd < cur_to {
                            if cur_to == u64::MAX { touched.push(e.to); }
                            unsafe { *dist.get_unchecked_mut(e.to as usize) = nd; }
                            pq.push(nd, e.to);
                        }
                    }
                }

                for &u in &touched { unsafe { *dist.get_unchecked_mut(u as usize) = u64::MAX; } }
                touched.clear();
                pq.clear();

                let ans = if best_t == u64::MAX { -1 } else { best_t as i64 };
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
