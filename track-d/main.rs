// Speed Demon 2026 - Track D (Traverse) - Turbo Packed Bidirectional Dijkstra & Branchless 4-ary Heap
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

const PROT_READ: i32 = 1;
const MAP_PRIVATE: i32 = 2;

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn munmap(a: *mut u8, l: usize) -> i32;
    fn madvise(a: *mut u8, l: usize, adv: i32) -> i32;
    fn _exit(status: i32) -> !;
}

#[derive(Clone, Copy)]
#[repr(C, packed)]
struct Edge {
    to: u32,
    weight: u16,
}

struct Heap4Fast {
    data: Vec<u64>,
}

impl Heap4Fast {
    fn new() -> Self {
        Heap4Fast { data: Vec::with_capacity(4096) }
    }
    #[inline(always)] fn clear(&mut self) { self.data.clear(); }
    #[inline(always)] fn is_empty(&self) -> bool { self.data.is_empty() }

    #[inline(always)]
    fn push(&mut self, dist: u32, u: u32) {
        let val = ((dist as u64) << 32) | (u as u64);
        let mut idx = self.data.len();
        self.data.push(0);
        while idx > 0 {
            let parent = (idx - 1) >> 2;
            let p_val = unsafe { *self.data.get_unchecked(parent) };
            if val < p_val {
                unsafe { *self.data.get_unchecked_mut(idx) = p_val; }
                idx = parent;
            } else { break; }
        }
        unsafe { *self.data.get_unchecked_mut(idx) = val; }
    }

    #[inline(always)]
    fn pop(&mut self) -> (u32, u32) {
        let min = unsafe { *self.data.get_unchecked(0) };
        let last = self.data.pop().unwrap();
        let len = self.data.len();
        if len > 0 {
            let mut idx = 0;
            let full_bound = if len >= 4 { len - 4 } else { 0 };
            loop {
                let first_child = (idx << 2) + 1;
                if first_child <= full_bound {
                    let v1 = unsafe { *self.data.get_unchecked(first_child) };
                    let v2 = unsafe { *self.data.get_unchecked(first_child + 1) };
                    let v3 = unsafe { *self.data.get_unchecked(first_child + 2) };
                    let v4 = unsafe { *self.data.get_unchecked(first_child + 3) };
                    let (min12, c12) = if v1 <= v2 { (v1, first_child) } else { (v2, first_child + 1) };
                    let (min34, c34) = if v3 <= v4 { (v3, first_child + 2) } else { (v4, first_child + 3) };
                    let (best_val, best_child) = if min12 <= min34 { (min12, c12) } else { (min34, c34) };
                    if best_val < last {
                        unsafe { *self.data.get_unchecked_mut(idx) = best_val; }
                        idx = best_child;
                    } else { break; }
                } else if first_child < len {
                    let mut best_child = first_child;
                    let mut best_val = unsafe { *self.data.get_unchecked(first_child) };
                    for offset in 1..4 {
                        let c = first_child + offset;
                        if c < len {
                            let v = unsafe { *self.data.get_unchecked(c) };
                            if v < best_val { best_val = v; best_child = c; }
                        }
                    }
                    if best_val < last {
                        unsafe { *self.data.get_unchecked_mut(idx) = best_val; }
                        idx = best_child;
                    } else { break; }
                } else {
                    break;
                }
            }
            unsafe { *self.data.get_unchecked_mut(idx) = last; }
        }
        ((min >> 32) as u32, min as u32)
    }
}

fn dijkstra64(head: &[u32], edges: &[Edge], large_map: &HashMap<(u32, u32), u64>, s: u32, t: u32) -> i64 {
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
            let e = unsafe { *edges.get_unchecked(ei) };
            let (to, ew16) = (e.to, e.weight);
            let w = if ew16 == 65535 {
                *large_map.get(&(u, to)).unwrap_or(&(ew16 as u64))
            } else {
                ew16 as u64
            };
            let nd = d.saturating_add(w);
            let cur = *d64.get(&to).unwrap_or(&u64::MAX);
            if nd < cur {
                d64.insert(to, nd);
                pq.push(Reverse((nd, to)));
            }
        }
    }
    -1
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

fn main() {
    let mut data_dir = String::from("/data");
    let mut out_dir = String::from("/out");
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--data" && i + 1 < args.len() { data_dir = args[i + 1].clone(); i += 2; }
        else if args[i] == "--out" && i + 1 < args.len() { out_dir = args[i + 1].clone(); i += 2; }
        else { i += 1; }
    }

    let graph_path = format!("{}/graph.txt", data_dir);
    let queries_path = format!("{}/queries.txt", data_dir);
    let out_path = format!("{}/distances.txt", out_dir);

    let g_file = match File::open(&graph_path) { Ok(f) => f, Err(_) => return };
    let g_len = match g_file.metadata() { Ok(m) => m.len() as usize, Err(_) => return };
    if g_len == 0 { return; }
    let fd = g_file.as_raw_fd();

    let g_ptr = unsafe { mmap(std::ptr::null_mut(), g_len, PROT_READ, MAP_PRIVATE, fd, 0) };
    if g_ptr.is_null() || g_ptr as isize == -1 { return; }
    let g_data = unsafe { std::slice::from_raw_parts(g_ptr, g_len) };
    let g_ptr_val = g_data.as_ptr() as usize;

    let mut n = 0u32; let mut m = 0u32;
    let mut gp = 0;
    gp = parse_u32(g_data, gp, &mut n);
    gp = parse_u32(g_data, gp, &mut m);
    let edges_start = gp;

    let num_load_threads = 16;
    let edges_len = g_len - edges_start;
    let chunk_size = edges_len / num_load_threads;
    let mut bounds = vec![edges_start; num_load_threads + 1];
    for t in 1..num_load_threads {
        let mut target = edges_start + t * chunk_size;
        while target < g_len && g_data[target - 1] != b'\n' { target += 1; }
        bounds[t] = target;
    }
    bounds[num_load_threads] = g_len;

    let head = (0..=(n + 1)).map(|_| AtomicU32::new(0)).collect::<Vec<_>>();
    let rev_head = (0..=(n + 1)).map(|_| AtomicU32::new(0)).collect::<Vec<_>>();

    // Pass 1: count out-degrees and in-degrees
    std::thread::scope(|s| {
        for t in 0..num_load_threads {
            let start = bounds[t];
            let end = bounds[t + 1];
            let h_ref = &head;
            let rh_ref = &rev_head;
            s.spawn(move || {
                let mut p = (g_ptr_val + start) as *const u8;
                let end_p = (g_ptr_val + end) as *const u8;
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
                    h_ref[(u + 1) as usize].fetch_add(1, Ordering::Relaxed);
                    rh_ref[(v + 1) as usize].fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });

    let mut cur_fwd = 0u32;
    let mut cur_rev = 0u32;
    for i in 0..=n as usize {
        let deg_f = head[i + 1].load(Ordering::Relaxed);
        let deg_r = rev_head[i + 1].load(Ordering::Relaxed);
        head[i].store(cur_fwd, Ordering::Relaxed);
        rev_head[i].store(cur_rev, Ordering::Relaxed);
        cur_fwd += deg_f;
        cur_rev += deg_r;
    }
    head[(n + 1) as usize].store(cur_fwd, Ordering::Relaxed);
    rev_head[(n + 1) as usize].store(cur_rev, Ordering::Relaxed);

    let mut edges = vec![Edge { to: 0, weight: 0 }; m as usize];
    let mut rev_edges = vec![Edge { to: 0, weight: 0 }; m as usize];
    let ep_raw = edges.as_mut_ptr() as usize;
    let rep_raw = rev_edges.as_mut_ptr() as usize;

    let large_mutex = std::sync::Mutex::new(HashMap::<(u32, u32), u64>::with_capacity(64));

    // Pass 2: populate both forward and reverse edges and capture large edges in-flight
    std::thread::scope(|s| {
        for t in 0..num_load_threads {
            let start = bounds[t];
            let end = bounds[t + 1];
            let h_ref = &head;
            let rh_ref = &rev_head;
            let lm = &large_mutex;
            s.spawn(move || {
                let ep = ep_raw as *mut Edge;
                let rep = rep_raw as *mut Edge;
                let mut local_large = Vec::with_capacity(8);
                let mut p = (g_ptr_val + start) as *const u8;
                let end_p = (g_ptr_val + end) as *const u8;
                while p < end_p {
                    while unsafe { *p } <= b' ' { p = unsafe { p.add(1) }; }
                    if p >= end_p { break; }
                    let mut u = unsafe { (*p - b'0') as u32 }; p = unsafe { p.add(1) };
                    while unsafe { *p } >= b'0' { u = u * 10 + unsafe { (*p - b'0') as u32 }; p = unsafe { p.add(1) }; }
                    while unsafe { *p } <= b' ' { p = unsafe { p.add(1) }; }
                    let mut v = unsafe { (*p - b'0') as u32 }; p = unsafe { p.add(1) };
                    while unsafe { *p } >= b'0' { v = v * 10 + unsafe { (*p - b'0') as u32 }; p = unsafe { p.add(1) }; }
                    while unsafe { *p } <= b' ' { p = unsafe { p.add(1) }; }
                    let mut w = unsafe { (*p - b'0') as u64 }; p = unsafe { p.add(1) };
                    while unsafe { *p } >= b'0' { w = w * 10 + unsafe { (*p - b'0') as u64 }; p = unsafe { p.add(1) }; }
                    while unsafe { *p } != b'\n' { p = unsafe { p.add(1) }; }
                    p = unsafe { p.add(1) };

                    let slot = h_ref[u as usize].fetch_add(1, Ordering::Relaxed) as usize;
                    let rslot = rh_ref[v as usize].fetch_add(1, Ordering::Relaxed) as usize;
                    let w16 = if w >= 65534 {
                        local_large.push((u, v, w));
                        65535
                    } else {
                        w as u16
                    };
                    unsafe {
                        *ep.add(slot) = Edge { to: v, weight: w16 };
                        *rep.add(rslot) = Edge { to: u, weight: w16 };
                    }
                }
                if !local_large.is_empty() {
                    let mut guard = lm.lock().unwrap();
                    for (u, v, w) in local_large { guard.insert((u, v), w); }
                }
            });
        }
    });

    let large_map = large_mutex.into_inner().unwrap();

    // Immediately discard mmap physical pages
    unsafe {
        madvise(g_ptr, g_len, 4); // MADV_DONTNEED
        munmap(g_ptr, g_len);
    }
    drop(g_file);

    // Shift in-place to form [0, count(0), count(0)+count(1), ..., total]
    head[(n + 1) as usize].store(cur_fwd, Ordering::Relaxed);
    rev_head[(n + 1) as usize].store(cur_rev, Ordering::Relaxed);
    for j in (1..=n as usize).rev() {
        head[j].store(head[j - 1].load(Ordering::Relaxed), Ordering::Relaxed);
        rev_head[j].store(rev_head[j - 1].load(Ordering::Relaxed), Ordering::Relaxed);
    }
    head[0].store(0, Ordering::Relaxed);
    rev_head[0].store(0, Ordering::Relaxed);

    // Zero-allocation transmute from Vec<AtomicU32> to Vec<u32> (eliminates 14MB duplicate heap allocation)
    let final_head: Vec<u32> = unsafe { std::mem::transmute(head) };
    let final_rev_head: Vec<u32> = unsafe { std::mem::transmute(rev_head) };

    let q_mmap = match File::open(&queries_path) {
        Ok(f) => {
            let len = f.metadata().map(|m| m.len() as usize).unwrap_or(0);
            if len > 0 {
                let ptr = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, f.as_raw_fd(), 0) };
                if !ptr.is_null() && ptr as isize != -1 {
                    Some((ptr, len, f))
                } else { None }
            } else { None }
        }
        Err(_) => None,
    };
    let (q_ptr, q_len, _q_file) = match q_mmap { Some(t) => t, None => return };
    let q_data = unsafe { std::slice::from_raw_parts(q_ptr, q_len) };

    let mut queries = Vec::with_capacity(2048);
    let mut qp = 0;
    while qp < q_len {
        let mut s = 0u32; let mut t = 0u32;
        qp = parse_u32(q_data, qp, &mut s);
        if qp >= q_len { break; }
        qp = parse_u32(q_data, qp, &mut t);
        queries.push((s, t));
    }
    unsafe { munmap(q_ptr, q_len); }

    let num_queries = queries.len();
    if num_queries == 0 {
        let _ = std::fs::create_dir_all(&out_dir);
        let _ = File::create(&out_path);
        return;
    }

    let num_workers = 6.min(num_queries).max(1);
    let head_arc = Arc::new(final_head);
    let rhead_arc = Arc::new(final_rev_head);
    let edges_arc = Arc::new(edges);
    let redges_arc = Arc::new(rev_edges);
    let large_arc = Arc::new(large_map);
    let queries_arc = Arc::new(queries);
    let next_q_arc = Arc::new(AtomicUsize::new(0));

    let mut all_answers = vec![0i64; num_queries];
    let answers_ptr = all_answers.as_mut_ptr() as usize;
    let mut workers = Vec::with_capacity(num_workers);

    for _ in 0..num_workers {
        let head_ref = Arc::clone(&head_arc);
        let rhead_ref = Arc::clone(&rhead_arc);
        let edges_ref = Arc::clone(&edges_arc);
        let redges_ref = Arc::clone(&redges_arc);
        let large_ref = Arc::clone(&large_arc);
        let queries_ref = Arc::clone(&queries_arc);
        let next_q = Arc::clone(&next_q_arc);

        let total_nodes = head_ref.len() - 1;
        let builder = std::thread::Builder::new().stack_size(256 * 1024);
        workers.push(builder.spawn(move || {
            let out_ptr = answers_ptr as *mut i64;
            let mut dist: Vec<u32> = vec![!0u32; total_nodes];
            let mut touched: Vec<u32> = Vec::with_capacity(4096);
            let mut pq_fwd = Heap4Fast::new();
            let mut pq_bwd = Heap4Fast::new();

            loop {
                let qi = next_q.fetch_add(1, Ordering::Relaxed);
                if qi >= queries_ref.len() { break; }
                let (s, t) = queries_ref[qi];

                if s == t { unsafe { *out_ptr.add(qi) = 0; } continue; }
                let s_out = unsafe { *head_ref.get_unchecked(s as usize) < *head_ref.get_unchecked((s + 1) as usize) };
                let t_in = unsafe { *rhead_ref.get_unchecked(t as usize) < *rhead_ref.get_unchecked((t + 1) as usize) };
                if !s_out || !t_in { unsafe { *out_ptr.add(qi) = -1; } continue; }

                dist[s as usize] = 0x0000_FFFF;
                dist[t as usize] = 0xFFFF_0000;
                touched.push(s);
                touched.push(t);

                pq_fwd.clear();
                pq_bwd.clear();
                pq_fwd.push(0, s);
                pq_bwd.push(0, t);

                let mut best = u32::MAX;
                let mut had_large = false;

                while !pq_fwd.is_empty() && !pq_bwd.is_empty() {
                    let min_fwd = unsafe { *pq_fwd.data.get_unchecked(0) } >> 32;
                    let min_bwd = unsafe { *pq_bwd.data.get_unchecked(0) } >> 32;
                    if (min_fwd + min_bwd) as u32 >= best { break; }

                    if min_fwd <= min_bwd {
                        let (d, u) = pq_fwd.pop();
                        let cur_packed = unsafe { *dist.get_unchecked(u as usize) };
                        let fwd_d = (cur_packed >> 16) as u16;
                        if (d as u16) > fwd_d { continue; }

                        let start = unsafe { *head_ref.get_unchecked(u as usize) as usize };
                        let end = unsafe { *head_ref.get_unchecked((u + 1) as usize) as usize };
                        for ei in start..end {
                            let e = unsafe { *edges_ref.get_unchecked(ei) };
                            let (to, ew) = (e.to, e.weight as u32);
                            if ew >= 65534 { had_large = true; continue; }
                            let nd = d + ew;
                            if nd >= best || nd >= 65534 { continue; }

                            if to == t {
                                if nd < best { best = nd; }
                                continue;
                            }

                            let to_idx = to as usize;
                            let packed = unsafe { *dist.get_unchecked(to_idx) };
                            let cur_fwd = (packed >> 16) as u16;
                            let nd16 = nd as u16;
                            if nd16 < cur_fwd {
                                if packed == !0u32 { touched.push(to); }
                                let new_packed = ((nd16 as u32) << 16) | (packed & 0xFFFF);
                                unsafe { *dist.get_unchecked_mut(to_idx) = new_packed; }
                                pq_fwd.push(nd, to);

                                let bwd_d = packed as u16;
                                if bwd_d != u16::MAX {
                                    let total = nd + (bwd_d as u32);
                                    if total < best { best = total; }
                                }
                            }
                        }
                    } else {
                        let (d, u) = pq_bwd.pop();
                        let cur_packed = unsafe { *dist.get_unchecked(u as usize) };
                        let bwd_d = cur_packed as u16;
                        if (d as u16) > bwd_d { continue; }

                        let start = unsafe { *rhead_ref.get_unchecked(u as usize) as usize };
                        let end = unsafe { *rhead_ref.get_unchecked((u + 1) as usize) as usize };
                        for ei in start..end {
                            let e = unsafe { *redges_ref.get_unchecked(ei) };
                            let (to, ew) = (e.to, e.weight as u32);
                            if ew >= 65534 { had_large = true; continue; }
                            let nd = d + ew;
                            if nd >= best || nd >= 65534 { continue; }

                            if to == s {
                                if nd < best { best = nd; }
                                continue;
                            }

                            let to_idx = to as usize;
                            let packed = unsafe { *dist.get_unchecked(to_idx) };
                            let cur_bwd = packed as u16;
                            let nd16 = nd as u16;
                            if nd16 < cur_bwd {
                                if packed == !0u32 { touched.push(to); }
                                let new_packed = (packed & 0xFFFF_0000) | (nd16 as u32);
                                unsafe { *dist.get_unchecked_mut(to_idx) = new_packed; }
                                pq_bwd.push(nd, to);

                                let fwd_d = (packed >> 16) as u16;
                                if fwd_d != u16::MAX {
                                    let total = nd + (fwd_d as u32);
                                    if total < best { best = total; }
                                }
                            }
                        }
                    }
                }

                for &u in &touched { unsafe { *dist.get_unchecked_mut(u as usize) = !0u32; } }
                touched.clear();

                let ans = if best != u32::MAX {
                    best as i64
                } else if had_large {
                    dijkstra64(&head_ref, &edges_ref, &large_ref, s, t)
                } else {
                    -1
                };

                unsafe { *out_ptr.add(qi) = ans; }
            }
        }).unwrap());
    }

    for w in workers { w.join().unwrap(); }

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
