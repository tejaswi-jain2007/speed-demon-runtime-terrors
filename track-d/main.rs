// Speed Demon 2026 - Track D (Traverse)
// Team: Runtime Terrors
// Multi-threaded compact CSR Dijkstra graph engine in Rust

use std::cmp::Reverse;
use std::collections::BinaryHeap;
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
        if ptr as isize == -1 {
            return None;
        }

        unsafe { madvise(ptr, len, MADV_WILLNEED) };
        Some(FastMmap {
            ptr,
            len,
            _file: file,
        })
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

#[inline(always)]
fn parse_u32(data: &[u8], mut p: usize) -> (u32, usize) {
    while p < data.len() && (data[p] == b' ' || data[p] == b'\n' || data[p] == b'\r') {
        p += 1;
    }
    let mut val: u32 = 0;
    while p < data.len() && data[p] >= b'0' && data[p] <= b'9' {
        val = val * 10 + (data[p] - b'0') as u32;
        p += 1;
    }
    (val, p)
}

struct CsrGraph {
    n: usize,
    head: Vec<u32>,
    to: Vec<u32>,
    weight: Vec<u32>,
}

unsafe impl Send for CsrGraph {}
unsafe impl Sync for CsrGraph {}

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

    // 1. Mmap graph.txt
    let g_mmap = match FastMmap::open(&graph_path) {
        Some(m) => m,
        None => {
            let _ = std::fs::create_dir_all(&out_dir);
            let _ = File::create(&out_path);
            return;
        }
    };

    let g_data = g_mmap.as_slice();
    let (n_val, mut gp) = parse_u32(g_data, 0);
    let (m_val, next_p) = parse_u32(g_data, gp);
    gp = next_p;

    let n = n_val as usize;
    let m = m_val as usize;

    let gp_edges_start = gp;
    let mut degrees = vec![0u32; n];

    // Pass 1: Count out-degrees
    let mut p = gp_edges_start;
    for _ in 0..m {
        let (u, next_p) = parse_u32(g_data, p);
        degrees[u as usize] += 1;
        // Skip v and w quickly to end of line
        p = next_p;
        while p < g_data.len() && g_data[p] != b'\n' {
            p += 1;
        }
        if p < g_data.len() {
            p += 1;
        }
    }

    // Build head prefix sums
    let mut head = vec![0u32; n + 1];
    for i in 0..n {
        head[i + 1] = head[i] + degrees[i];
    }

    let mut cur_offsets = head[..n].to_vec();
    let mut to = vec![0u32; m];
    let mut weight = vec![0u32; m];

    // Pass 2: Fill CSR arrays
    p = gp_edges_start;
    for _ in 0..m {
        let (u, p1) = parse_u32(g_data, p);
        let (v, p2) = parse_u32(g_data, p1);
        let (w, p3) = parse_u32(g_data, p2);
        p = p3;

        let slot = cur_offsets[u as usize] as usize;
        to[slot] = v;
        weight[slot] = w;
        cur_offsets[u as usize] += 1;
    }

    let graph = Arc::new(CsrGraph { n, head, to, weight });

    // 2. Read queries.txt
    let q_mmap = match FastMmap::open(&queries_path) {
        Some(m) => m,
        None => {
            let _ = std::fs::create_dir_all(&out_dir);
            let _ = File::create(&out_path);
            return;
        }
    };

    let q_data = q_mmap.as_slice();
    let mut queries = Vec::with_capacity(4096);
    let mut qp = 0;
    while qp < q_data.len() {
        while qp < q_data.len() && (q_data[qp] == b' ' || q_data[qp] == b'\n' || q_data[qp] == b'\r') {
            qp += 1;
        }
        if qp >= q_data.len() {
            break;
        }
        let (s, p1) = parse_u32(q_data, qp);
        let (t, p2) = parse_u32(q_data, p1);
        qp = p2;
        queries.push((s, t));
    }

    let num_queries = queries.len();
    let num_threads = 12.min(thread::available_parallelism().map(|k| k.get()).unwrap_or(12)).max(1);
    let chunk_size = (num_queries + num_threads - 1) / num_threads;

    let mut handles = Vec::with_capacity(num_threads);
    let queries_arc = Arc::new(queries);

    for t in 0..num_threads {
        let g = Arc::clone(&graph);
        let q_ref = Arc::clone(&queries_arc);
        let start_idx = t * chunk_size;
        let end_idx = num_queries.min(start_idx + chunk_size);

        handles.push(thread::spawn(move || {
            let mut results = Vec::with_capacity(end_idx - start_idx);
            let mut dist = vec![u32::MAX; g.n];
            let mut visited_epoch = vec![0u32; g.n];
            let mut current_epoch: u32 = 0;
            let mut heap = BinaryHeap::new();

            for i in start_idx..end_idx {
                let (s, target) = q_ref[i];
                if s == target {
                    results.push(0i64);
                    continue;
                }

                current_epoch += 1;
                if current_epoch == 0 {
                    // Overflow wrap-around (virtually impossible in 2000 queries, but safe)
                    visited_epoch.fill(0);
                    current_epoch = 1;
                }

                heap.clear();
                dist[s as usize] = 0;
                visited_epoch[s as usize] = current_epoch;
                heap.push(Reverse((0u32, s)));

                let mut ans = -1i64;
                while let Some(Reverse((d, u))) = heap.pop() {
                    if u == target {
                        ans = d as i64;
                        break;
                    }
                    if d > dist[u as usize] {
                        continue;
                    }

                    let u_start = g.head[u as usize] as usize;
                    let u_end = g.head[u as usize + 1] as usize;
                    for edge_idx in u_start..u_end {
                        let v = g.to[edge_idx];
                        let w = g.weight[edge_idx];
                        let next_d = d + w;

                        let v_usize = v as usize;
                        if visited_epoch[v_usize] != current_epoch || next_d < dist[v_usize] {
                            dist[v_usize] = next_d;
                            visited_epoch[v_usize] = current_epoch;
                            heap.push(Reverse((next_d, v)));
                        }
                    }
                }
                results.push(ans);
            }
            results
        }));
    }

    let mut out_buffer = Vec::with_capacity(128 * 1024);
    for handle in handles {
        let thread_results = handle.join().unwrap();
        for ans in thread_results {
            out_buffer.extend_from_slice(ans.to_string().as_bytes());
            out_buffer.push(b'\n');
        }
    }

    let _ = std::fs::create_dir_all(&out_dir);
    if let Ok(mut f) = File::create(&out_path) {
        let _ = f.write_all(&out_buffer);
    }
}
