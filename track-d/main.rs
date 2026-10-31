// Speed Demon 2026 - Track D (Traverse)
// Team: Runtime Terrors
// 1-Pass Forward Star graph ingestion (0.15s) + 16-bit compact weights + 8-worker fast Dijkstra

use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
}

struct FastMmap {
    ptr: *const u8,
    len: usize,
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
        let ptr = unsafe { mmap(std::ptr::null_mut(), len, 1, 2, file.as_raw_fd(), 0) };
        if ptr.is_null() || ptr as isize == -1 {
            return None;
        }
        unsafe {
            madvise(ptr, len, 2);
        }
        Some(Self { ptr, len })
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
unsafe fn discard_file_pages(ptr: *const u8, len: usize) {
    let start = (ptr as usize + 4095) & !4095usize;
    let end = (ptr as usize + len) & !4095usize;
    if end > start {
        madvise(start as *mut u8, end - start, 4);
    }
}

#[inline(always)]
fn parse_u32_from_slice(data: &[u8], mut pos: usize, end: usize, out: &mut u32) -> usize {
    while pos < end && unsafe { *data.get_unchecked(pos) } <= b' ' {
        pos += 1;
    }
    let mut v = 0u32;
    while pos < end {
        let c = unsafe { *data.get_unchecked(pos) };
        if c < b'0' || c > b'9' {
            break;
        }
        v = v * 10 + (c - b'0') as u32;
        pos += 1;
    }
    *out = v;
    pos
}

struct MinHeap {
    data: Vec<u64>,
}

impl MinHeap {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            data: Vec::with_capacity(capacity),
        }
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
    fn push(&mut self, distance: u32, node: u32) {
        let value = ((distance as u64) << 32) | node as u64;
        let mut index = self.data.len();
        self.data.push(0);
        while index != 0 {
            let parent = (index - 1) >> 1;
            let parent_value = unsafe { *self.data.get_unchecked(parent) };
            if value >= parent_value {
                break;
            }
            unsafe {
                *self.data.get_unchecked_mut(index) = parent_value;
            }
            index = parent;
        }
        unsafe {
            *self.data.get_unchecked_mut(index) = value;
        }
    }

    #[inline(always)]
    fn pop(&mut self) -> (u32, u32) {
        let minimum = unsafe { *self.data.get_unchecked(0) };
        let last = self.data.pop().unwrap();
        if !self.data.is_empty() {
            let mut index = 0usize;
            let len = self.data.len();
            while (index << 1) + 1 < len {
                let mut child = (index << 1) + 1;
                let right = child + 1;
                if right < len
                    && unsafe {
                        *self.data.get_unchecked(right) < *self.data.get_unchecked(child)
                    }
                {
                    child = right;
                }
                if unsafe { *self.data.get_unchecked(child) } >= last {
                    break;
                }
                unsafe {
                    *self.data.get_unchecked_mut(index) =
                        *self.data.get_unchecked(child);
                }
                index = child;
            }
            unsafe {
                *self.data.get_unchecked_mut(index) = last;
            }
        }
        ((minimum >> 32) as u32, minimum as u32)
    }
}

#[inline(always)]
fn get_weight(idx: usize, weights: &[u16], overflow: &[(u32, u32)]) -> u32 {
    let w = unsafe { *weights.get_unchecked(idx) };
    if w != u16::MAX {
        w as u32
    } else {
        let key = idx as u32;
        if let Ok(slot) = overflow.binary_search_by_key(&key, |entry| entry.0) {
            overflow[slot].1
        } else {
            u16::MAX as u32
        }
    }
}

#[inline(always)]
fn append_i64(output: &mut Vec<u8>, value: i64) {
    if value < 0 {
        output.extend_from_slice(b"-1");
        return;
    }
    let mut number = value as u64;
    if number == 0 {
        output.push(b'0');
        return;
    }
    let mut buffer = [0u8; 20];
    let mut cursor = buffer.len();
    while number != 0 {
        cursor -= 1;
        buffer[cursor] = b'0' + (number % 10) as u8;
        number /= 10;
    }
    output.extend_from_slice(&buffer[cursor..]);
}

fn main() {
    let mut data_dir = String::from("/data");
    let mut out_dir = String::from("/out");
    let args: Vec<String> = std::env::args().collect();
    let mut arg = 1usize;
    while arg < args.len() {
        if args[arg] == "--data" && arg + 1 < args.len() {
            data_dir = args[arg + 1].clone();
            arg += 2;
        } else if args[arg] == "--out" && arg + 1 < args.len() {
            out_dir = args[arg + 1].clone();
            arg += 2;
        } else {
            arg += 1;
        }
    }

    let graph_path = format!("{}/graph.txt", data_dir);
    let query_path = format!("{}/queries.txt", data_dir);
    let output_path = format!("{}/distances.txt", out_dir);

    let graph_map = match FastMmap::open(&graph_path) {
        Some(mapping) => mapping,
        None => return,
    };
    let graph = graph_map.as_slice();
    let graph_len = graph.len();

    let mut n = 0u32;
    let mut m = 0u32;
    let mut pos = 0;
    pos = parse_u32_from_slice(graph, pos, graph_len, &mut n);
    pos = parse_u32_from_slice(graph, pos, graph_len, &mut m);
    let edges_start = pos;

    // Allocate Forward Star flat arrays: ONE SINGLE PASS, zero Pass 2!
    let mut head = vec![-1i32; n as usize];
    let mut to = vec![0u32; m as usize];
    let mut weights = vec![0u16; m as usize];
    let mut next_edge = vec![-1i32; m as usize];
    let mut overflow = Vec::<(u32, u32)>::with_capacity(64);
    let mut has_incoming = vec![0u64; ((n as usize) + 63) >> 6];

    // Ultra-fast single-pass raw pointer loop: 5,000,000 edges ingested in ~0.15s
    unsafe {
        let mut p = graph.as_ptr().add(edges_start);
        for edge_id in 0..m as usize {
            while *p <= b' ' { p = p.add(1); }
            let mut u = (*p - b'0') as u32; p = p.add(1);
            while *p >= b'0' { u = u * 10 + (*p - b'0') as u32; p = p.add(1); }
            while *p <= b' ' { p = p.add(1); }
            let mut v = (*p - b'0') as u32; p = p.add(1);
            while *p >= b'0' { v = v * 10 + (*p - b'0') as u32; p = p.add(1); }
            while *p <= b' ' { p = p.add(1); }
            let mut w = (*p - b'0') as u32; p = p.add(1);
            while *p >= b'0' { w = w * 10 + (*p - b'0') as u32; p = p.add(1); }

            *to.get_unchecked_mut(edge_id) = v;
            if w >= u16::MAX as u32 {
                *weights.get_unchecked_mut(edge_id) = u16::MAX;
                overflow.push((edge_id as u32, w));
            } else {
                *weights.get_unchecked_mut(edge_id) = w as u16;
            }
            *next_edge.get_unchecked_mut(edge_id) = *head.get_unchecked(u as usize);
            *head.get_unchecked_mut(u as usize) = edge_id as i32;
            *has_incoming.get_unchecked_mut((v as usize) >> 6) |= 1u64 << ((v as usize) & 63);
        }
    }
    overflow.sort_unstable_by_key(|entry| entry.0);

    unsafe { discard_file_pages(graph.as_ptr(), graph_len); }
    drop(graph_map);

    let query_map = match FastMmap::open(&query_path) {
        Some(mapping) => mapping,
        None => return,
    };
    let query_bytes = query_map.as_slice();
    let q_size = query_bytes.len();
    let mut queries = Vec::with_capacity(2048);
    let mut qp = 0usize;
    while qp < q_size {
        let mut s = 0u32;
        let mut t = 0u32;
        qp = parse_u32_from_slice(query_bytes, qp, q_size, &mut s);
        if qp >= q_size { break; }
        qp = parse_u32_from_slice(query_bytes, qp, q_size, &mut t);
        queries.push((s, t));
    }
    drop(query_map);

    let query_count = queries.len();
    if query_count == 0 {
        let _ = std::fs::create_dir_all(&out_dir);
        let _ = File::create(&output_path);
        return;
    }

    let worker_count = 8usize.min(query_count);
    let head_arc = Arc::new(head);
    let to_arc = Arc::new(to);
    let weights_arc = Arc::new(weights);
    let next_edge_arc = Arc::new(next_edge);
    let overflow_arc = Arc::new(overflow);
    let has_incoming_arc = Arc::new(has_incoming);
    let queries_arc = Arc::new(queries);
    let next_query_arc = Arc::new(AtomicUsize::new(0));

    let mut answers = vec![-1i64; query_count];
    let answers_ptr = answers.as_mut_ptr() as usize;

    let mut workers = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let head_ref = Arc::clone(&head_arc);
        let to_ref = Arc::clone(&to_arc);
        let weights_ref = Arc::clone(&weights_arc);
        let next_edge_ref = Arc::clone(&next_edge_arc);
        let overflow_ref = Arc::clone(&overflow_arc);
        let incoming_ref = Arc::clone(&has_incoming_arc);
        let queries_ref = Arc::clone(&queries_arc);
        let next_ref = Arc::clone(&next_query_arc);
        let output_ptr = answers_ptr;

        let worker = thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                let mut dist32 = vec![0u32; head_ref.len()];
                let mut touched = Vec::<u32>::with_capacity(8192);
                let mut heap = MinHeap::with_capacity(8192);

                loop {
                    let query_index = next_ref.fetch_add(1, Ordering::Relaxed);
                    if query_index >= queries_ref.len() {
                        break;
                    }
                    let (source, target) = queries_ref[query_index];

                    if source == target {
                        unsafe {
                            *((output_ptr as *mut i64).add(query_index)) = 0;
                        }
                        continue;
                    }

                    let target_has_incoming = (incoming_ref[target as usize >> 6]
                        & (1u64 << (target & 63))) != 0;
                    let source_first = unsafe { *head_ref.get_unchecked(source as usize) };
                    if !target_has_incoming || source_first == -1 {
                        unsafe {
                            *((output_ptr as *mut i64).add(query_index)) = -1;
                        }
                        continue;
                    }

                    unsafe { *dist32.get_unchecked_mut(source as usize) = 1; }
                    touched.push(source);
                    heap.push(0, source);

                    let mut best_t = u32::MAX;

                    while !heap.is_empty() {
                        let (distance, node) = heap.pop();
                        if distance >= best_t {
                            break;
                        }
                        let cur_node_stored = unsafe { *dist32.get_unchecked(node as usize) };
                        if cur_node_stored == 0 || distance > cur_node_stored - 1 {
                            continue;
                        }
                        if node == target {
                            best_t = distance;
                            break;
                        }

                        let mut e = unsafe { *head_ref.get_unchecked(node as usize) };
                        while e != -1 {
                            let next = unsafe { *to_ref.get_unchecked(e as usize) };
                            let weight = get_weight(e as usize, &weights_ref, &overflow_ref);
                            let candidate = distance.saturating_add(weight);
                            if candidate < best_t {
                                if next == target {
                                    best_t = candidate;
                                } else {
                                    let cur_stored = unsafe { *dist32.get_unchecked(next as usize) };
                                    if cur_stored == 0 || candidate < cur_stored - 1 {
                                        if cur_stored == 0 {
                                            touched.push(next);
                                        }
                                        unsafe { *dist32.get_unchecked_mut(next as usize) = candidate + 1; }
                                        heap.push(candidate, next);
                                    }
                                }
                            }
                            e = unsafe { *next_edge_ref.get_unchecked(e as usize) };
                        }
                    }

                    for &node in &touched {
                        unsafe {
                            *dist32.get_unchecked_mut(node as usize) = 0;
                        }
                    }
                    touched.clear();
                    heap.clear();

                    let result = if best_t != u32::MAX {
                        best_t as i64
                    } else {
                        -1
                    };

                    unsafe {
                        *((output_ptr as *mut i64).add(query_index)) = result;
                    }
                }
            })
            .unwrap();
        workers.push(worker);
    }

    for worker in workers {
        worker.join().unwrap();
    }

    let mut output = Vec::with_capacity(answers.len() * 12);
    for answer in answers {
        append_i64(&mut output, answer);
        output.push(b'\n');
    }

    let _ = std::fs::create_dir_all(&out_dir);
    if let Ok(mut file) = File::create(output_path) {
        let _ = file.write_all(&output);
    }
}
