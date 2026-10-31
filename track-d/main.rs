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
    fn bytes(&self) -> &[u8] {
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
fn parse_u32(data: &[u8], mut pos: usize, end: usize) -> (u32, usize) {
    while pos < end && data[pos] <= b' ' {
        pos += 1;
    }
    let mut value = 0u32;
    while pos < end {
        let c = unsafe { *data.get_unchecked(pos) };
        if c < b'0' || c > b'9' {
            break;
        }
        value = value * 10 + (c - b'0') as u32;
        pos += 1;
    }
    (value, pos)
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
fn get_weight(index: usize, weights: &[u16], overflow: &[(u32, u32)]) -> u32 {
    let compact = unsafe { *weights.get_unchecked(index) };
    if compact != u16::MAX {
        compact as u32
    } else {
        let key = index as u32;
        let slot = overflow
            .binary_search_by_key(&key, |entry| entry.0)
            .expect("overflow edge index missing");
        overflow[slot].1
    }
}

fn dijkstra_u64(
    offsets: &[u32],
    targets: &[u32],
    weights: &[u16],
    overflow: &[(u32, u32)],
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
        let begin = offsets[node as usize] as usize;
        let end = offsets[node as usize + 1] as usize;
        for edge_index in begin..end {
            let next = unsafe { *targets.get_unchecked(edge_index) };
            let weight = get_weight(edge_index, weights, overflow) as u64;
            let candidate = distance + weight;
            if candidate < *distances.get(&next).unwrap_or(&u64::MAX) {
                distances.insert(next, candidate);
                heap.push(Reverse((candidate, next)));
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
        Some(mapping) => mapping,
        None => return,
    };
    let graph = graph_map.bytes();
    let graph_len = graph.len();

    let (n, mut pos) = parse_u32(graph, 0, graph_len);
    let (m, next) = parse_u32(graph, pos, graph_len);
    pos = next;

    let mut offsets = vec![0u32; n as usize + 1];
    let mut has_incoming = vec![0u64; (n as usize + 63) >> 6];

    let mut scan = pos;
    for _ in 0..m {
        let (u, p1) = parse_u32(graph, scan, graph_len);
        let (v, p2) = parse_u32(graph, p1, graph_len);
        let (_, p3) = parse_u32(graph, p2, graph_len);
        scan = p3;
        offsets[u as usize + 1] += 1;
        has_incoming[v as usize >> 6] |= 1u64 << (v & 63);
    }

    for node in 0..n as usize {
        offsets[node + 1] += offsets[node];
    }

    let mut targets = vec![0u32; m as usize];
    let mut weights = vec![0u16; m as usize];
    let mut overflow = Vec::<(u32, u32)>::with_capacity(128);

    scan = pos;
    for _ in 0..m {
        let (u, p1) = parse_u32(graph, scan, graph_len);
        let (v, p2) = parse_u32(graph, p1, graph_len);
        let (weight, p3) = parse_u32(graph, p2, graph_len);
        scan = p3;

        let edge_index = offsets[u as usize] as usize;
        offsets[u as usize] += 1;
        targets[edge_index] = v;
        if weight >= u16::MAX as u32 {
            weights[edge_index] = u16::MAX;
            overflow.push((edge_index as u32, weight));
        } else {
            weights[edge_index] = weight as u16;
        }
    }

    for node in (0..n as usize).rev() {
        offsets[node + 1] = offsets[node];
    }
    offsets[0] = 0;
    overflow.sort_unstable_by_key(|entry| entry.0);

    unsafe {
        discard_file_pages(graph.as_ptr(), graph_len);
    }
    drop(graph_map);

    let query_map = match FastMmap::open(&query_path) {
        Some(mapping) => mapping,
        None => return,
    };
    let query_bytes = query_map.bytes();
    let mut queries = Vec::<(u32, u32)>::with_capacity(2048);
    let mut query_pos = 0usize;
    while query_pos < query_bytes.len() {
        while query_pos < query_bytes.len() && query_bytes[query_pos] <= b' ' {
            query_pos += 1;
        }
        if query_pos == query_bytes.len() {
            break;
        }
        let (source, p1) = parse_u32(query_bytes, query_pos, query_bytes.len());
        let (target, p2) = parse_u32(query_bytes, p1, query_bytes.len());
        queries.push((source, target));
        query_pos = p2;
    }
    drop(query_map);

    let query_count = queries.len();
    if query_count == 0 {
        let _ = std::fs::create_dir_all(&out_dir);
        let _ = File::create(&output_path);
        return;
    }

    let worker_count = 6usize.min(query_count);
    let offsets = Arc::new(offsets);
    let targets = Arc::new(targets);
    let weights = Arc::new(weights);
    let overflow = Arc::new(overflow);
    let has_incoming = Arc::new(has_incoming);
    let queries = Arc::new(queries);
    let next_query = Arc::new(AtomicUsize::new(0));
    let mut answers = vec![-1i64; query_count];
    let answers_ptr = answers.as_mut_ptr() as usize;

    let mut workers = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let offsets_ref = Arc::clone(&offsets);
        let targets_ref = Arc::clone(&targets);
        let weights_ref = Arc::clone(&weights);
        let overflow_ref = Arc::clone(&overflow);
        let incoming_ref = Arc::clone(&has_incoming);
        let queries_ref = Arc::clone(&queries);
        let next_ref = Arc::clone(&next_query);
        let output_ptr = answers_ptr;

        let worker = thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                let mut distances = vec![u32::MAX; offsets_ref.len() - 1];
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

                    let target_has_incoming = incoming_ref[target as usize >> 6]
                        & (1u64 << (target & 63))
                        != 0;
                    let source_begin = offsets_ref[source as usize];
                    let source_end = offsets_ref[source as usize + 1];
                    if !target_has_incoming || source_begin == source_end {
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

                        let begin = offsets_ref[node as usize] as usize;
                        let end = offsets_ref[node as usize + 1] as usize;
                        for edge_index in begin..end {
                            let next = unsafe { *targets_ref.get_unchecked(edge_index) };
                            let weight = get_weight(edge_index, &weights_ref, &overflow_ref);
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
                            &offsets_ref,
                            &targets_ref,
                            &weights_ref,
                            &overflow_ref,
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
