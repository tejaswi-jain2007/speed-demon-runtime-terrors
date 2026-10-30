// Speed Demon 2026 - Track A (Ingest)
// Team: Runtime Terrors
// High-performance zero-dependency multi-threaded log aggregation engine in Rust

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

// 64-bit FNV-1a hash for fast grouping without allocations
#[inline(always)]
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3u64);
    }
    h
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

struct LocalGroup {
    hour: [u8; 13],
    endpoint_offset: u32,
    endpoint_len: u16,
    count: u32,
    errors: u32,
    latencies: Vec<u32>,
}

fn parse_chunk(data: &[u8], start: usize, end: usize) -> Vec<LocalGroup> {
    const TABLE_SIZE: usize = 2048;
    const MASK: usize = TABLE_SIZE - 1;
    let mut table_keys = [0u64; TABLE_SIZE];
    let mut table_indices = [u32::MAX; TABLE_SIZE];
    let mut groups: Vec<LocalGroup> = Vec::with_capacity(256);

    let mut p = start;
    let chunk_end = end;

    while p < chunk_end {
        if p + 28 > chunk_end {
            break;
        }

        // Timestamp is 27 bytes: YYYY-MM-DDTHH:MM:SS.ffffffZ
        // Hour prefix is first 13 bytes: YYYY-MM-DDTHH
        let mut hour_bytes = [0u8; 13];
        hour_bytes.copy_from_slice(&data[p..p + 13]);
        p += 28; // skip timestamp and space

        // Method: scan until space
        while p < chunk_end && data[p] != b' ' {
            p += 1;
        }
        if p >= chunk_end {
            break;
        }
        p += 1; // skip space

        // Endpoint: scan until space
        let ep_start = p;
        while p < chunk_end && data[p] != b' ' {
            p += 1;
        }
        if p >= chunk_end {
            break;
        }
        let ep_len = p - ep_start;
        p += 1; // skip space

        // Status code: 3 ASCII digits
        if p + 3 > chunk_end {
            break;
        }
        let status = (data[p] - b'0') as u32 * 100
            + (data[p + 1] - b'0') as u32 * 10
            + (data[p + 2] - b'0') as u32;
        p += 3;
        if p < chunk_end && data[p] == b' ' {
            p += 1;
        }

        // Latency in microseconds: ASCII decimal integer
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

        // Composite hash: hour XOR endpoint
        let mut key_hash = fnv1a_64(&hour_bytes);
        key_hash ^= fnv1a_64(&data[ep_start..ep_start + ep_len]);

        let mut slot = (key_hash as usize) & MASK;
        let mut group_idx = u32::MAX;

        loop {
            if table_indices[slot] == u32::MAX {
                let new_idx = groups.len() as u32;
                groups.push(LocalGroup {
                    hour: hour_bytes,
                    endpoint_offset: ep_start as u32,
                    endpoint_len: ep_len as u16,
                    count: 0,
                    errors: 0,
                    latencies: Vec::with_capacity(8192),
                });
                table_keys[slot] = key_hash;
                table_indices[slot] = new_idx;
                group_idx = new_idx;
                break;
            } else if table_keys[slot] == key_hash {
                let g = &groups[table_indices[slot] as usize];
                if g.hour == hour_bytes && &data[g.endpoint_offset as usize..g.endpoint_offset as usize + g.endpoint_len as usize] == &data[ep_start..ep_start + ep_len] {
                    group_idx = table_indices[slot];
                    break;
                }
            }
            slot = (slot + 1) & MASK;
        }

        let grp = &mut groups[group_idx as usize];
        grp.count += 1;
        if status >= 500 && status <= 599 {
            grp.errors += 1;
        }
        grp.latencies.push(lat);
    }

    groups
}

struct FinalGroup {
    hour: String,
    endpoint: String,
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
            let d = mmap_ref.as_slice();
            parse_chunk(d, start, end)
        }));
    }

    let mut canonical_groups: BTreeMap<(String, String), (u64, u64, Vec<u32>)> = BTreeMap::new();
    for handle in handles {
        let thread_groups = handle.join().unwrap();
        for g in thread_groups {
            let hr_str = unsafe { std::str::from_utf8_unchecked(&g.hour) }.to_string();
            let ep_str = unsafe {
                std::str::from_utf8_unchecked(&data[g.endpoint_offset as usize..g.endpoint_offset as usize + g.endpoint_len as usize])
            }.to_string();

            let entry = canonical_groups.entry((hr_str, ep_str)).or_insert_with(|| (0, 0, Vec::new()));
            entry.0 += g.count as u64;
            entry.1 += g.errors as u64;
            entry.2.extend_from_slice(&g.latencies);
        }
    }

    let final_list: Vec<FinalGroup> = canonical_groups
        .into_iter()
        .map(|((hour, endpoint), (count, errors, mut latencies))| {
            latencies.sort_unstable();
            let n = count as usize;
            let r50 = (50 * n + 99) / 100;
            let r95 = (95 * n + 99) / 100;
            let r99 = (99 * n + 99) / 100;
            let p50 = latencies[r50 - 1];
            let p95 = latencies[r95 - 1];
            let p99 = latencies[r99 - 1];
            let error_ppm = if count > 0 { (errors * 1_000_000) / count } else { 0 };

            FinalGroup {
                hour,
                endpoint,
                count,
                errors,
                p50,
                p95,
                p99,
                error_ppm,
            }
        })
        .collect();

    let mut out_buffer = Vec::with_capacity(1024 * 1024);
    for g in &final_list {
        out_buffer.extend_from_slice(g.hour.as_bytes());
        out_buffer.extend_from_slice(b":00:00Z ");
        out_buffer.extend_from_slice(g.endpoint.as_bytes());
        out_buffer.push(b' ');
        out_buffer.extend_from_slice(g.count.to_string().as_bytes());
        out_buffer.push(b' ');
        out_buffer.extend_from_slice(g.p50.to_string().as_bytes());
        out_buffer.push(b' ');
        out_buffer.extend_from_slice(g.p95.to_string().as_bytes());
        out_buffer.push(b' ');
        out_buffer.extend_from_slice(g.p99.to_string().as_bytes());
        out_buffer.push(b' ');
        out_buffer.extend_from_slice(g.errors.to_string().as_bytes());
        out_buffer.push(b' ');
        out_buffer.extend_from_slice(g.error_ppm.to_string().as_bytes());
        out_buffer.push(b'\n');
    }

    let _ = std::fs::create_dir_all(&out_dir);
    if let Ok(mut f) = File::create(&out_path) {
        let _ = f.write_all(&out_buffer);
    }
}
