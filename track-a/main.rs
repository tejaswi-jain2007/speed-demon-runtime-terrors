// Speed Demon 2026 - Track A (Ingest)
// Team: Runtime Terrors
// Ultra-high-performance zero-dependency log aggregation engine in Rust

use std::collections::BTreeMap;
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
        if ptr == std::ptr::null_mut() || ptr as isize == -1 {
            return None;
        }
        unsafe {
            madvise(ptr, len, MADV_WILLNEED);
        }
        Some(FastMmap { ptr, len, _file: file })
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
fn append_u64(buf: &mut Vec<u8>, mut v: u64) {
    if v == 0 {
        buf.push(b'0');
        return;
    }
    let mut temp = [0u8; 20];
    let mut idx = 20;
    while v > 0 {
        idx -= 1;
        temp[idx] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    buf.extend_from_slice(&temp[idx..]);
}

struct LocalGroup {
    count: u32,
    errors: u32,
    latencies: Vec<u32>,
}

struct ThreadParser {
    hours: Vec<[u8; 13]>,
    endpoints: Vec<Vec<u8>>,
    table: Vec<Vec<LocalGroup>>,
}

impl ThreadParser {
    fn new() -> Self {
        ThreadParser {
            hours: Vec::with_capacity(64),
            endpoints: Vec::with_capacity(32),
            table: Vec::with_capacity(64),
        }
    }
}

fn parse_chunk(data: &[u8], start: usize, end: usize, parser: &mut ThreadParser) {
    let mut p = start;
    let chunk_end = end;

    let mut cur_hour_id = usize::MAX;
    let mut last_hour = [0u8; 13];

    while p < chunk_end {
        if p + 28 > chunk_end {
            break;
        }

        // Fast hour caching
        let hour_slice = &data[p..p + 13];
        if cur_hour_id == usize::MAX || hour_slice != &last_hour {
            last_hour.copy_from_slice(hour_slice);
            cur_hour_id = usize::MAX;
            for (i, h) in parser.hours.iter().enumerate() {
                if h == &last_hour {
                    cur_hour_id = i;
                    break;
                }
            }
            if cur_hour_id == usize::MAX {
                cur_hour_id = parser.hours.len();
                parser.hours.push(last_hour);
                let mut row = Vec::with_capacity(parser.endpoints.len());
                for _ in 0..parser.endpoints.len() {
                    row.push(LocalGroup { count: 0, errors: 0, latencies: Vec::new() });
                }
                parser.table.push(row);
            }
        }
        p += 28; // skip 27-byte timestamp + space

        // Method: scan until space
        while p < chunk_end && data[p] != b' ' {
            p += 1;
        }
        if p >= chunk_end {
            break;
        }
        p += 1;

        // Endpoint: scan until space
        let ep_start = p;
        while p < chunk_end && data[p] != b' ' {
            p += 1;
        }
        if p >= chunk_end {
            break;
        }
        let ep_len = p - ep_start;
        p += 1;

        // Endpoint caching
        let ep_slice = &data[ep_start..ep_start + ep_len];
        let mut cur_ep_id = usize::MAX;
        for (i, ep) in parser.endpoints.iter().enumerate() {
            if ep.as_slice() == ep_slice {
                cur_ep_id = i;
                break;
            }
        }
        if cur_ep_id == usize::MAX {
            cur_ep_id = parser.endpoints.len();
            parser.endpoints.push(ep_slice.to_vec());
            for row in &mut parser.table {
                row.push(LocalGroup { count: 0, errors: 0, latencies: Vec::new() });
            }
        }

        // Status: 3 ASCII digits
        if p + 3 > chunk_end {
            break;
        }
        let status = (data[p] - b'0') as u32 * 100
            + (data[p + 1] - b'0') as u32 * 10
            + (data[p + 2] - b'0') as u32;
        let is_error = status >= 500 && status <= 599;
        p += 3;
        if p < chunk_end && data[p] == b' ' {
            p += 1;
        }

        // Latency in microseconds
        let mut lat: u32 = 0;
        while p < chunk_end && data[p] >= b'0' && data[p] <= b'9' {
            lat = lat * 10 + (data[p] - b'0') as u32;
            p += 1;
        }
        if p < chunk_end && data[p] == b'\r' {
            p += 1;
        }
        if p < chunk_end && data[p] == b'\n' {
            p += 1;
        }

        // Direct 2D grid lookup: zero hash overhead, direct L1 access
        let grp = unsafe {
            parser.table.get_unchecked_mut(cur_hour_id).get_unchecked_mut(cur_ep_id)
        };
        grp.count += 1;
        if is_error {
            grp.errors += 1;
        }
        grp.latencies.push(lat);
    }
}

struct OutputGroup {
    hour: [u8; 13],
    endpoint: Vec<u8>,
    count: u64,
    errors: u64,
    p50: u32,
    p95: u32,
    p99: u32,
    error_ppm: u64,
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

    let in_path = format!("{}/requests.log", data_dir);
    let out_path = format!("{}/summary.txt", out_dir);

    let mmap = match FastMmap::open(&in_path) {
        Some(m) => Arc::new(m),
        None => {
            let _ = std::fs::create_dir_all(&out_dir);
            let _ = File::create(&out_path);
            return;
        }
    };

    let data = mmap.as_slice();
    let file_size = data.len();
    let num_threads = 12.min(thread::available_parallelism().map(|n| n.get()).unwrap_or(12)).max(1);

    let mut splits = vec![0usize; num_threads + 1];
    splits[num_threads] = file_size;
    for t in 1..num_threads {
        let mut pt = t * (file_size / num_threads);
        while pt < file_size && data[pt] != b'\n' {
            pt += 1;
        }
        if pt < file_size {
            pt += 1;
        }
        splits[t] = pt;
    }

    let mut handles = Vec::with_capacity(num_threads);
    for t in 0..num_threads {
        let mmap_ref = Arc::clone(&mmap);
        let start = splits[t];
        let end = splits[t + 1];
        handles.push(thread::spawn(move || {
            let mut parser = ThreadParser::new();
            let d = mmap_ref.as_slice();
            parse_chunk(d, start, end, &mut parser);
            parser
        }));
    }

    // Merge into canonical BTreeMap sorted by (hour, endpoint)
    let mut merged: BTreeMap<([u8; 13], Vec<u8>), (u64, u64, Vec<u32>)> = BTreeMap::new();
    for handle in handles {
        let mut parser = handle.join().unwrap();
        for hid in 0..parser.hours.len() {
            for eid in 0..parser.endpoints.len() {
                let grp = &mut parser.table[hid][eid];
                if grp.count > 0 {
                    let entry = merged.entry((parser.hours[hid], parser.endpoints[eid].clone()))
                        .or_insert_with(|| (0, 0, Vec::new()));
                    entry.0 += grp.count as u64;
                    entry.1 += grp.errors as u64;
                    if entry.2.is_empty() {
                        entry.2 = std::mem::take(&mut grp.latencies);
                    } else {
                        entry.2.append(&mut grp.latencies);
                    }
                }
            }
        }
    }

    // Parallel O(N) Quickselect percentiles using select_nth_unstable
    struct TaskItem {
        hour: [u8; 13],
        endpoint: Vec<u8>,
        count: u64,
        errors: u64,
        latencies: Vec<u32>,
    }

    let items: Vec<TaskItem> = merged.into_iter().map(|((hour, endpoint), (count, errors, latencies))| {
        TaskItem { hour, endpoint, count, errors, latencies }
    }).collect();

    let total_groups = items.len();
    let sort_threads = num_threads.min(total_groups).max(1);
    let per_worker = (total_groups + sort_threads - 1) / sort_threads;

    let items_arc = Arc::new(items);
    let mut sort_handles = Vec::with_capacity(sort_threads);

    for t in 0..sort_threads {
        let start_idx = t * per_worker;
        let end_idx = (start_idx + per_worker).min(total_groups);
        let items_ref = Arc::clone(&items_arc);

        sort_handles.push(thread::spawn(move || {
            let mut local_results = Vec::with_capacity(end_idx - start_idx);
            for i in start_idx..end_idx {
                let item = &items_ref[i];
                let count = item.count;
                if count == 0 {
                    continue;
                }
                let mut lats = item.latencies.clone();
                let n = count as usize;
                let r50 = (50 * n + 99) / 100 - 1;
                let r95 = (95 * n + 99) / 100 - 1;
                let r99 = (99 * n + 99) / 100 - 1;

                lats.select_nth_unstable(r50);
                let p50 = lats[r50];

                let p95 = if r95 > r50 {
                    lats[r50 + 1..].select_nth_unstable(r95 - r50 - 1);
                    lats[r95]
                } else {
                    p50
                };

                let p99 = if r99 > r95 {
                    lats[r95 + 1..].select_nth_unstable(r99 - r95 - 1);
                    lats[r99]
                } else {
                    p95
                };

                let error_ppm = (item.errors * 1_000_000) / count;

                local_results.push(OutputGroup {
                    hour: item.hour,
                    endpoint: item.endpoint.clone(),
                    count,
                    errors: item.errors,
                    p50,
                    p95,
                    p99,
                    error_ppm,
                });
            }
            local_results
        }));
    }

    let mut final_groups = Vec::with_capacity(total_groups);
    for sh in sort_handles {
        final_groups.extend(sh.join().unwrap());
    }

    // High-speed buffered I/O write
    let mut out_buffer = Vec::with_capacity(2 * 1024 * 1024);
    for g in &final_groups {
        out_buffer.extend_from_slice(&g.hour);
        out_buffer.extend_from_slice(b":00:00Z ");
        out_buffer.extend_from_slice(&g.endpoint);
        out_buffer.push(b' ');
        append_u64(&mut out_buffer, g.count);
        out_buffer.push(b' ');
        append_u64(&mut out_buffer, g.p50 as u64);
        out_buffer.push(b' ');
        append_u64(&mut out_buffer, g.p95 as u64);
        out_buffer.push(b' ');
        append_u64(&mut out_buffer, g.p99 as u64);
        out_buffer.push(b' ');
        append_u64(&mut out_buffer, g.errors);
        out_buffer.push(b' ');
        append_u64(&mut out_buffer, g.error_ppm);
        out_buffer.push(b'\n');
    }

    let _ = std::fs::create_dir_all(&out_dir);
    if let Ok(mut f) = File::create(&out_path) {
        let _ = f.write_all(&out_buffer);
    }
}
