// Speed Demon 2026 - Track D (Traverse) - Ultra Packed Bidirectional Dijkstra & 4-ary Heap
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn munmap(a: *mut u8, l: usize) -> i32;
    fn madvise(a: *mut u8, l: usize, adv: i32) -> i32;
    fn _exit(status: i32) -> !;
}

#[derive(Clone, Copy)]
#[repr(C, packed)]
struct Edge { to: u32, weight: u16 }

struct FastMmap { ptr: *const u8, len: usize, _file: File }
unsafe impl Send for FastMmap {}
unsafe impl Sync for FastMmap {}

impl FastMmap {
    fn open(path: &str) -> Option<Self> {
        let file = File::open(path).ok()?;
        let len = file.metadata().ok()?.len() as usize;
        if len == 0 { return None; }
        let fd = file.as_raw_fd();
        let ptr = unsafe { mmap(std::ptr::null_mut(), len, 1, 2, fd, 0) };
        if ptr.is_null() || ptr as isize == -1 { return None; }
        unsafe { madvise(ptr, len, 2); }
        Some(FastMmap { ptr, len, _file: file })
    }
    #[inline(always)] fn as_slice(&self) -> &[u8] { unsafe { std::slice::from_raw_parts(self.ptr, self.len) } }
}
impl Drop for FastMmap { fn drop(&mut self) { unsafe { munmap(self.ptr as *mut u8, self.len); } } }

#[inline(always)]
unsafe fn discard_file_pages(ptr: *const u8, len: usize) {
    let p = ptr as usize;
    let (s, e) = ((p + 4095) & !4095, (p + len) & !4095);
    if e > s { madvise(s as *mut u8, e - s, 4); }
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

struct Heap4 { data: Vec<u64> }
impl Heap4 {
    fn new() -> Self { Heap4 { data: Vec::with_capacity(8192) } }
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
        if !self.data.is_empty() {
            let mut idx = 0;
            let len = self.data.len();
            loop {
                let first_child = (idx << 2) + 1;
                if first_child >= len { break; }
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
            let e = unsafe { edges.get_unchecked(ei) };
            let (to, w16) = (e.to, e.weight);
            let w = if w16 == 65535 { *large_map.get(&(u, to)).unwrap_or(&65535) } else { w16 as u64 };
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

    let g_mmap = match FastMmap::open(&graph_path) { Some(m) => m, None => return };
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
        while target < g_data.len() && g_data[target - 1] != b'\n' { target += 1; }
        bounds[t] = target;
    }
    bounds[num_load_threads] = g_data.len();

    let head = (0..=(n + 1)).map(|_| AtomicU32::new(0)).collect::<Vec<_>>();
    let rev_head = (0..=(n + 1)).map(|_| AtomicU32::new(0)).collect::<Vec<_>>();

    std::thread::scope(|s| {
        for t in 0..num_load_threads {
            let start = bounds[t];
            let end = bounds[t + 1];
            let h_ref = &head;
            let rh_ref = &rev_head;
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
    let ep = SyncEdges(edges.as_mut_ptr());
    let rep = SyncEdges(rev_edges.as_mut_ptr());

    std::thread::scope(|s| {
        for t in 0..num_load_threads {
            let start = bounds[t];
            let end = bounds[t + 1];
            let h_ref = &head;
            let rh_ref = &rev_head;
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
                    let mut w = unsafe { (*p - b'0') as u64 }; p = unsafe { p.add(1) };
                    while unsafe { *p } >= b'0' { w = w * 10 + unsafe { (*p - b'0') as u64 }; p = unsafe { p.add(1) }; }
                    while unsafe { *p } != b'\n' { p = unsafe { p.add(1) }; }
                    p = unsafe { p.add(1) };

                    let slot = h_ref[u as usize].fetch_add(1, Ordering::Relaxed) as usize;
                    let rslot = rh_ref[v as usize].fetch_add(1, Ordering::Relaxed) as usize;
                    let w16 = if w >= 65534 { 65535 } else { w as u16 };
                    unsafe {
                        *ep.0.add(slot) = Edge { to: v, weight: w16 };
                        *rep.0.add(rslot) = Edge { to: u, weight: w16 };
                    }
                }
            });
        }
    });

    let mut final_head = vec![0u32; (n + 2) as usize];
    let mut final_rev_head = vec![0u32; (n + 2) as usize];
    final_head[0] = 0; final_rev_head[0] = 0;
    for j in 0..n as usize {
        final_head[j + 1] = head[j].load(Ordering::Relaxed);
        final_rev_head[j + 1] = rev_head[j].load(Ordering::Relaxed);
    }
    final_head[(n + 1) as usize] = cur_fwd;
    final_rev_head[(n + 1) as usize] = cur_rev;
    drop(head); drop(rev_head);

    let mut large_map: HashMap<(u32, u32), u64> = HashMap::new();
    let mut p = edges_start;
    while p < g_data.len() {
        while p < g_data.len() && g_data[p] <= b' ' { p += 1; }
        if p >= g_data.len() { break; }
        let mut u = 0u32; let mut v = 0u32;
        p = parse_u32(g_data, p, &mut u);
        p = parse_u32(g_data, p, &mut v);
        while p < g_data.len() && g_data[p] <= b' ' { p += 1; }
        let mut w = 0u64;
        while p < g_data.len() && g_data[p] >= b'0' { w = w * 10 + (g_data[p] - b'0') as u64; p += 1; }
        while p < g_data.len() && g_data[p] != b'\n' { p += 1; }
        if w >= 65534 { large_map.insert((u, v), w); }
    }
    unsafe { discard_file_pages(g_data.as_ptr(), g_data.len()); }
    drop(g_mmap);

    let q_mmap = match FastMmap::open(&queries_path) { Some(m) => m, None => return };
    let q_data = q_mmap.as_slice();
    let mut queries = Vec::new();
    let mut qp = 0;
    while qp < q_data.len() {
        let mut s = 0u32; let mut t = 0u32;
        qp = parse_u32(q_data, qp, &mut s);
        if qp >= q_data.len() { break; }
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

    let num_workers = 8.min(num_queries).max(1);
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

        let builder = std::thread::Builder::new().stack_size(256 * 1024);
        workers.push(builder.spawn(move || {
            let out_ptr = answers_ptr as *mut i64;
            let mut dist = vec![!0u32; head_ref.len() - 1];
            let mut touched: Vec<u32> = Vec::with_capacity(8192);
            let mut pq_fwd = Heap4::new();
            let mut pq_bwd = Heap4::new();

            loop {
                let qi = next_q.fetch_add(1, Ordering::Relaxed);
                if qi >= queries_ref.len() { break; }
                let (s, t) = queries_ref[qi];

                if s == t { unsafe { *out_ptr.add(qi) = 0; } continue; }
                let s_out = unsafe { *head_ref.get_unchecked(s as usize) < *head_ref.get_unchecked((s + 1) as usize) };
                let t_in = unsafe { *rhead_ref.get_unchecked(t as usize) < *rhead_ref.get_unchecked((t + 1) as usize) };
                if !s_out || !t_in { unsafe { *out_ptr.add(qi) = -1; } continue; }

                dist[s as usize] = 0x0000_FFFF;
                touched.push(s);
                pq_fwd.push(0, s);

                dist[t as usize] = (dist[t as usize] & 0xFFFF_0000) | 0x0000;
                if dist[t as usize] == 0xFFFF_0000 { touched.push(t); }
                pq_bwd.push(0, t);

                let mut best = u32::MAX;
                let mut had_large = false;

                while !pq_fwd.is_empty() && !pq_bwd.is_empty() {
                    let top_f = (unsafe { *pq_fwd.data.get_unchecked(0) } >> 32) as u32;
                    let top_b = (unsafe { *pq_bwd.data.get_unchecked(0) } >> 32) as u32;
                    if top_f.saturating_add(top_b) >= best { break; }

                    if pq_fwd.data.len() <= pq_bwd.data.len() {
                        let (d, u) = pq_fwd.pop();
                        if d >= best { break; }
                        let cur = (dist[u as usize] >> 16) as u32;
                        if d > cur { continue; }

                        let start = unsafe { *head_ref.get_unchecked(u as usize) as usize };
                        let end = unsafe { *head_ref.get_unchecked((u + 1) as usize) as usize };
                        for ei in start..end {
                            let e = unsafe { *edges_ref.get_unchecked(ei) };
                            let (to, ew) = (e.to, e.weight as u32);
                            if ew >= 65534 { had_large = true; continue; }
                            let nd = d + ew;
                            if nd >= best || nd >= 65534 { continue; }

                            let packed = dist[to as usize];
                            let cur_fwd = (packed >> 16) as u16;
                            let nd16 = nd as u16;
                            if nd16 < cur_fwd {
                                if packed == !0u32 { touched.push(to); }
                                dist[to as usize] = ((nd16 as u32) << 16) | (packed & 0xFFFF);
                                pq_fwd.push(nd, to);

                                let bwd_d = packed as u16;
                                if bwd_d != u16::MAX {
                                    let total = nd + bwd_d as u32;
                                    if total < best { best = total; }
                                }
                            }
                        }
                    } else {
                        let (d, u) = pq_bwd.pop();
                        if d >= best { break; }
                        let cur = (dist[u as usize] & 0xFFFF) as u32;
                        if d > cur { continue; }

                        let start = unsafe { *rhead_ref.get_unchecked(u as usize) as usize };
                        let end = unsafe { *rhead_ref.get_unchecked((u + 1) as usize) as usize };
                        for ei in start..end {
                            let e = unsafe { *redges_ref.get_unchecked(ei) };
                            let (to, ew) = (e.to, e.weight as u32);
                            if ew >= 65534 { had_large = true; continue; }
                            let nd = d + ew;
                            if nd >= best || nd >= 65534 { continue; }

                            let packed = dist[to as usize];
                            let cur_bwd = packed as u16;
                            let nd16 = nd as u16;
                            if nd16 < cur_bwd {
                                if packed == !0u32 { touched.push(to); }
                                dist[to as usize] = (packed & 0xFFFF_0000) | (nd16 as u32);
                                pq_bwd.push(nd, to);

                                let fwd_d = (packed >> 16) as u16;
                                if fwd_d != u16::MAX {
                                    let total = nd + fwd_d as u32;
                                    if total < best { best = total; }
                                }
                            }
                        }
                    }
                }

                for &u in &touched { dist[u as usize] = !0u32; }
                touched.clear();
                pq_fwd.clear();
                pq_bwd.clear();

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
