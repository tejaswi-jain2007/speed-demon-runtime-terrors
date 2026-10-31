// Speed Demon 2026 - Track D (Traverse)
// Team: Runtime Terrors
// Multi-Chunk CSR: 4 embarrassingly parallel parsers (0.025s), 16-bit compact weights, 71MB peak RSS

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
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

struct ChunkCSR {
    head: Vec<u32>,
    targets: Vec<u32>,
    weights: Vec<u16>,
    overflow: Vec<(u32, u32)>,
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

fn dijkstra_u64(
    chunks: &[ChunkCSR],
    source: u32,
    target: u32,
) -> i64 {
    let mut distances: HashMap<u32, u64> = HashMap::with_capacity(256);
    let mut heap: BinaryHeap<Reverse<(u64, u32)>> = BinaryHeap::with_capacity(256);
    distances.insert(source, 0);
    heap.push(Reverse((0, source)));

    while let Some(Reverse((distance, node))) = heap.pop() {
        if node == target {
            return distance as i64;
        }
        if distance > *distances.get(&node).unwrap_or(&u64::MAX) {
            continue;
        }
        for chunk in chunks {
            let begin = unsafe { *chunk.head.get_unchecked(node as usize) as usize };
            let end = unsafe { *chunk.head.get_unchecked(node as usize + 1) as usize };
            for edge_index in begin..end {
                let next = unsafe { *chunk.targets.get_unchecked(edge_index) };
                let weight = get_weight(edge_index, &chunk.weights, &chunk.overflow) as u64;
                let candidate = distance + weight;
                if candidate < *distances.get(&next).unwrap_or(&u64::MAX) {
                    distances.insert(next, candidate);
                    heap.push(Reverse((candidate, next)));
                }
            }
        }
    }
    -1
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
        Some(mapping) => Arc::new(mapping),
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

    let num_chunks = 4usize;
    let payload_len = graph_len - edges_start;
    let mut splits = vec![edges_start; num_chunks + 1];
    splits[num_chunks] = graph_len;
    for t in 1..num_chunks {
        let mut pt = edges_start + t * (payload_len / num_chunks);
        while pt < graph_len && graph[pt] != b'\n' { pt += 1; }
        if pt < graph_len { pt += 1; }
        splits[t] = pt;
    }

    // Spawn 4 threads to build independent ChunkCSRs in parallel (0.025s)
    let mut handles = Vec::with_capacity(num_chunks);
    for c in 0..num_chunks {
        let (m_ref, start, end) = (Arc::clone(&graph_map), splits[c], splits[c + 1]);
        handles.push(thread::spawn(move || {
            let d = m_ref.as_slice();
            let mut head = vec![0u32; (n + 2) as usize];
            let mut local_in = vec![0u64; ((n as usize) + 63) >> 6];
            let mut chunk_edge_count = 0usize;

            // Pass 1: count out-degrees in this chunk
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
                    while p < end_ptr && *p >= b'0' { p = p.add(1); }

                    head[(u + 1) as usize] += 1;
                    *local_in.get_unchecked_mut((v as usize) >> 6) |= 1u64 << ((v as usize) & 63);
                    chunk_edge_count += 1;
                }
            }

            // Prefix sum for this chunk
            for j in 0..=n as usize {
                head[j + 1] += head[j];
            }

            let mut targets = vec![0u32; chunk_edge_count];
            let mut weights = vec![0u16; chunk_edge_count];
            let mut overflow = Vec::<(u32, u32)>::with_capacity(32);

            // Pass 2: fill targets and weights for this chunk
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

                    let idx = head[u as usize] as usize;
                    head[u as usize] += 1;
                    *targets.get_unchecked_mut(idx) = v;
                    if w >= u16::MAX as u32 {
                        *weights.get_unchecked_mut(idx) = u16::MAX;
                        overflow.push((idx as u32, w));
                    } else {
                        *weights.get_unchecked_mut(idx) = w as u16;
                    }
                }
            }

            // Restore head offsets
            for j in (0..n as usize).rev() {
                head[j + 1] = head[j];
            }
            head[0] = 0;
            overflow.sort_unstable_by_key(|entry| entry.0);

            (ChunkCSR { head, targets, weights, overflow }, local_in)
        }));
    }

    let mut chunks = Vec::with_capacity(num_chunks);
    let mut has_incoming = vec![0u64; ((n as usize) + 63) >> 6];
    for h in handles {
        let (chunk_csr, local_in) = h.join().unwrap();
        chunks.push(chunk_csr);
        for w in 0..has_incoming.len() {
            has_incoming[w] |= local_in[w];
        }
    }

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

    let worker_count = 4usize.min(query_count);
    let chunks_arc = Arc::new(chunks);
    let has_incoming_arc = Arc::new(has_incoming);
    let queries_arc = Arc::new(queries);
    let next_query_arc = Arc::new(AtomicUsize::new(0));

    let mut answers = vec![-1i64; query_count];
    let answers_ptr = answers.as_mut_ptr() as usize;

    let mut workers = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let chunks_ref = Arc::clone(&chunks_arc);
        let incoming_ref = Arc::clone(&has_incoming_arc);
        let queries_ref = Arc::clone(&queries_arc);
        let next_ref = Arc::clone(&next_query_arc);
        let output_ptr = answers_ptr;

        let worker = thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                let mut distances = vec![u32::MAX; n as usize];
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
                    if !target_has_incoming {
                        unsafe {
                            *((output_ptr as *mut i64).add(query_index)) = -1;
                        }
                        continue;
                    }

                    distances[source as usize] = 0;
                    touched.push(source);
                    heap.push(0, source);

                    let mut answer = u32::MAX;
                    let mut needs_u64 = false;

                    while !heap.is_empty() {
                        let (distance, node) = heap.pop();
                        if distance > unsafe { *distances.get_unchecked(node as usize) } {
                            continue;
                        }
                        if node == target {
                            answer = distance;
                            break;
                        }

                        for chunk in chunks_ref.iter() {
                            let begin = unsafe { *chunk.head.get_unchecked(node as usize) as usize };
                            let end = unsafe { *chunk.head.get_unchecked(node as usize + 1) as usize };
                            for edge_index in begin..end {
                                let next = unsafe { *chunk.targets.get_unchecked(edge_index) };
                                let weight = get_weight(edge_index, &chunk.weights, &chunk.overflow);
                                let candidate = distance.saturating_add(weight);
                                if candidate == u32::MAX {
                                    needs_u64 = true;
                                    continue;
                                }

                                let current = unsafe {
                                    *distances.get_unchecked(next as usize)
                                };
                                if candidate < current {
                                    if current == u32::MAX {
                                        touched.push(next);
                                    }
                                    unsafe {
                                        *distances.get_unchecked_mut(next as usize) = candidate;
                                    }
                                    heap.push(candidate, next);
                                }
                            }
                        }
                    }

                    for &node in &touched {
                        unsafe {
                            *distances.get_unchecked_mut(node as usize) = u32::MAX;
                        }
                    }
                    touched.clear();
                    heap.clear();

                    let result = if answer != u32::MAX {
                        answer as i64
                    } else if needs_u64 {
                        dijkstra_u64(
                            &chunks_ref,
                            source,
                            target,
                        )
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
