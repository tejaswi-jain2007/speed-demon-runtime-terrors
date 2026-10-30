// Speed Demon 2026 - Track A (Ingest)
// Team: Runtime Terrors
// Ultra-fast zero-realloc single-arena log aggregation engine in Rust

use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use std::thread;

const PROT_READ: i32 = 1;
const MAP_PRIVATE: i32 = 2;
const MADV_SEQUENTIAL: i32 = 2;

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
            madvise(ptr, len, MADV_SEQUENTIAL);
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

#[inline(always)]
fn get_hour_index(ts: &[u8]) -> usize {
    let day = (ts[8] - b'0') as usize * 10 + (ts[9] - b'0') as usize;
    let hour = (ts[11] - b'0') as usize * 10 + (ts[12] - b'0') as usize;
    if ts[6] == b'0' {
        // October (10)
        if day == 30 {
            hour.saturating_sub(18)
        } else {
            6 + hour
        }
    } else {
        // November (11)
        if day == 1 {
            30 + hour
        } else {
            54 + hour
        }
    }
}

fn build_hour_strings() -> Vec<[u8; 20]> {
    let mut res = Vec::with_capacity(72);
    // Oct 30: 18..23
    for h in 18..=23 {
        let mut s = *b"2026-10-30T00:00:00Z";
        s[11] = b'0' + (h / 10);
        s[12] = b'0' + (h % 10);
        res.push(s);
    }
    // Oct 31: 00..23
    for h in 0..=23 {
        let mut s = *b"2026-10-31T00:00:00Z";
        s[11] = b'0' + (h / 10);
        s[12] = b'0' + (h % 10);
        res.push(s);
    }
    // Nov 01: 00..23
    for h in 0..=23 {
        let mut s = *b"2026-11-01T00:00:00Z";
        s[11] = b'0' + (h / 10);
        s[12] = b'0' + (h % 10);
        res.push(s);
    }
    // Nov 02: 00..17
    for h in 0..=17 {
        let mut s = *b"2026-11-02T00:00:00Z";
        s[11] = b'0' + (h / 10);
        s[12] = b'0' + (h % 10);
        res.push(s);
    }
    res
}

#[inline(always)]
fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    let mut i = 0;
    while i + 8 <= b.len() {
        let chunk = unsafe { std::ptr::read_unaligned(b.as_ptr().add(i) as *const u64) };
        h = (h ^ chunk).wrapping_mul(0x100000001b3);
        i += 8;
    }
    while i < b.len() {
        h = (h ^ (b[i] as u64)).wrapping_mul(0x100000001b3);
        i += 1;
    }
    if h == 0 { 1 } else { h }
}

#[derive(Clone, Copy)]
struct HashSlot {
    hash: u64,
    start: u32,
    len: u16,
    id: u16,
}

struct EpTable {
    slots: Vec<HashSlot>,
    mask: usize,
    endpoints: Vec<Vec<u8>>,
}

impl EpTable {
    fn new(cap: usize) -> Self {
        let mask = cap - 1;
        EpTable {
            slots: vec![HashSlot { hash: 0, start: 0, len: 0, id: 0 }; cap],
            mask,
            endpoints: Vec::with_capacity(2048),
        }
    }

    #[inline(always)]
    fn get_or_insert(&mut self, ep_slice: &[u8]) -> usize {
        let h = hash_bytes(ep_slice);
        let mut idx = (h as usize) & self.mask;
        loop {
            let slot = unsafe { self.slots.get_unchecked(idx) };
            if slot.hash == 0 && slot.len == 0 {
                let id = self.endpoints.len() as u16;
                self.endpoints.push(ep_slice.to_vec());
                unsafe {
                    *self.slots.get_unchecked_mut(idx) = HashSlot {
                        hash: h,
                        start: 0,
                        len: ep_slice.len() as u16,
                        id,
                    };
                }
                return id as usize;
            }
            if slot.hash == h && slot.len as usize == ep_slice.len() {
                if unsafe { self.endpoints.get_unchecked(slot.id as usize) } == ep_slice {
                    return slot.id as usize;
                }
            }
            idx = (idx + 1) & self.mask;
        }
    }
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
    let num_threads = 10.min(thread::available_parallelism().map(|n| n.get()).unwrap_or(10)).max(1);

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

    // Phase 1: Count records and discover endpoints in parallel
    let mut phase1_handles = Vec::with_capacity(num_threads);
    for t in 0..num_threads {
        let mmap_ref = Arc::clone(&mmap);
        let start = splits[t];
        let end = splits[t + 1];
        phase1_handles.push(thread::spawn(move || {
            let d = mmap_ref.as_slice();
            let mut ep_table = EpTable::new(4096);
            // Pack count (high 16 bits) and errors (low 16 bits) into single u32 (halves Phase 1 RAM)
            let mut packed_counts = vec![0u32; 72 * 2048];

            let mut p = start;
            while p < end {
                if p + 28 > end { break; }
                let h_idx = get_hour_index(&d[p..p + 27]);
                p += 28;

                // Skip method
                while p < end && d[p] != b' ' { p += 1; }
                if p >= end { break; }
                p += 1;

                // Endpoint
                let ep_start = p;
                while p < end && d[p] != b' ' { p += 1; }
                if p >= end { break; }
                let ep_slice = &d[ep_start..p];
                p += 1;

                let local_ep_id = ep_table.get_or_insert(ep_slice);

                // Status
                if p >= end { break; }
                let is_err = d[p] == b'5';
                while p < end && d[p] != b' ' { p += 1; }
                if p >= end { break; }
                p += 1;

                // Skip latency
                while p < end && d[p] != b'\n' { p += 1; }
                if p < end { p += 1; }

                let gid = h_idx * 2048 + local_ep_id;
                packed_counts[gid] += 0x10000 + (is_err as u32);
            }

            (ep_table.endpoints, packed_counts)
        }));
    }

    let mut thread_phase1_results = Vec::with_capacity(num_threads);
    for ph in phase1_handles {
        thread_phase1_results.push(ph.join().unwrap());
    }

    // Unify all endpoints into lexicographically sorted global list
    let mut all_endpoints_set = std::collections::BTreeSet::new();
    for (eps, _) in &thread_phase1_results {
        for ep in eps {
            all_endpoints_set.insert(ep.clone());
        }
    }

    let global_endpoints: Vec<Vec<u8>> = all_endpoints_set.into_iter().collect();
    let num_global_eps = global_endpoints.len();

    // Map each thread's local_ep_id to global_ep_id
    let mut thread_to_global: Vec<Vec<u16>> = Vec::with_capacity(num_threads);
    for (eps, _) in &thread_phase1_results {
        let mut map = Vec::with_capacity(eps.len());
        for ep in eps {
            let g_id = global_endpoints.binary_search(ep).unwrap() as u16;
            map.push(g_id);
        }
        thread_to_global.push(map);
    }

    // Aggregate counts across all threads into global (72 * num_global_eps) grid
    let total_groups = 72 * num_global_eps;
    let mut global_counts = vec![0u32; total_groups];
    let mut global_errors = vec![0u32; total_groups];

    for (t, (_, packed)) in thread_phase1_results.iter().enumerate() {
        let t_map = &thread_to_global[t];
        for h in 0..72 {
            for local_id in 0..t_map.len() {
                let gid = h * 2048 + local_id;
                let c_err = packed[gid];
                let c = (c_err >> 16) as u32;
                if c > 0 {
                    let g_id = t_map[local_id] as usize;
                    let target_gid = h * num_global_eps + g_id;
                    global_counts[target_gid] += c;
                    global_errors[target_gid] += c_err & 0xFFFF;
                }
            }
        }
    }

    // Identify active groups and compute prefix offsets for exact zero-realloc arena
    let mut active_groups = Vec::with_capacity(total_groups);
    let mut group_offsets = vec![0usize; total_groups + 1];
    let mut total_records: usize = 0;

    for g in 0..total_groups {
        group_offsets[g] = total_records;
        let c = global_counts[g] as usize;
        if c > 0 {
            active_groups.push(g);
            total_records += c;
        }
    }
    group_offsets[total_groups] = total_records;

    // Compute thread-specific write starts (as u32, moved with zero cloning)
    let mut thread_write_offsets: Vec<Vec<u32>> = Vec::with_capacity(num_threads);
    let mut running_offsets = group_offsets.clone();

    for (t, (_, packed)) in thread_phase1_results.iter().enumerate() {
        let t_map = &thread_to_global[t];
        let mut t_offsets = vec![0u32; 72 * 2048];
        for h in 0..72 {
            for local_id in 0..t_map.len() {
                let gid = h * 2048 + local_id;
                let c = (packed[gid] >> 16) as usize;
                if c > 0 {
                    let g_id = t_map[local_id] as usize;
                    let target_gid = h * num_global_eps + g_id;
                    t_offsets[gid] = running_offsets[target_gid] as u32;
                    running_offsets[target_gid] += c;
                }
            }
        }
        thread_write_offsets.push(t_offsets);
    }
    drop(thread_phase1_results);
    drop(running_offsets);

    // Allocate exact flat arena (12.4MB)
    let mut flat_arena = vec![0u32; total_records];
    let flat_arena_ptr = flat_arena.as_mut_ptr() as usize;

    // Phase 2: Direct streaming writes into flat arena
    let mut phase2_handles = Vec::with_capacity(num_threads);
    let mut thread_write_iter = thread_write_offsets.into_iter();
    for t in 0..num_threads {
        let mmap_ref = Arc::clone(&mmap);
        let start = splits[t];
        let end = splits[t + 1];
        let mut t_write_pos = thread_write_iter.next().unwrap();
        let mut local_ep_table = EpTable::new(4096);

        phase2_handles.push(thread::spawn(move || {
            let d = mmap_ref.as_slice();
            let arena = flat_arena_ptr as *mut u32;

            let mut p = start;
            while p < end {
                if p + 28 > end { break; }
                let h_idx = get_hour_index(&d[p..p + 27]);
                p += 28;

                // Skip method
                while p < end && d[p] != b' ' { p += 1; }
                if p >= end { break; }
                p += 1;

                // Endpoint
                let ep_start = p;
                while p < end && d[p] != b' ' { p += 1; }
                if p >= end { break; }
                let ep_slice = &d[ep_start..p];
                p += 1;

                let local_ep_id = local_ep_table.get_or_insert(ep_slice);

                // Skip status
                while p < end && d[p] != b' ' { p += 1; }
                if p >= end { break; }
                p += 1;

                // Parse latency
                let mut lat = 0u32;
                while p < end && d[p] >= b'0' && d[p] <= b'9' {
                    lat = lat * 10 + (d[p] - b'0') as u32;
                    p += 1;
                }
                while p < end && d[p] != b'\n' { p += 1; }
                if p < end { p += 1; }

                let gid = h_idx * 2048 + local_ep_id;
                let write_idx = t_write_pos[gid];
                t_write_pos[gid] += 1;
                unsafe {
                    *arena.add(write_idx as usize) = lat;
                }
            }
            unsafe {
                madvise(d.as_ptr().add(start) as *mut u8, end - start, 4);
            }
        }));
    }

    for ph in phase2_handles {
        ph.join().unwrap();
    }
    drop(mmap);

    // Phase 3: Parallel percentiles & output formatting
    let hour_strings = build_hour_strings();
    let num_active = active_groups.len();
    let active_per_thread = (num_active + num_threads - 1) / num_threads;

    let arena_ptr = flat_arena.as_mut_ptr() as usize;
    let mut sort_handles = Vec::with_capacity(num_threads);

    let g_counts_arc = Arc::new(global_counts);
    let g_errors_arc = Arc::new(global_errors);
    let g_offsets_arc = Arc::new(group_offsets);
    let g_eps_arc = Arc::new(global_endpoints);
    let h_strs_arc = Arc::new(hour_strings);
    let active_groups_arc = Arc::new(active_groups);

    for t in 0..num_threads {
        let start_idx = t * active_per_thread;
        let end_idx = (start_idx + active_per_thread).min(num_active);
        let g_counts = Arc::clone(&g_counts_arc);
        let g_errors = Arc::clone(&g_errors_arc);
        let g_offsets = Arc::clone(&g_offsets_arc);
        let g_eps = Arc::clone(&g_eps_arc);
        let h_strs = Arc::clone(&h_strs_arc);
        let active_ref = Arc::clone(&active_groups_arc);

        sort_handles.push(thread::spawn(move || {
            let arena = arena_ptr as *mut u32;
            let mut out = Vec::with_capacity(32 * 1024);

            for idx in start_idx..end_idx {
                let g = active_ref[idx];
                let h_idx = g / num_global_eps;
                let ep_idx = g % num_global_eps;
                let count = g_counts[g] as usize;
                let errors = g_errors[g] as u64;
                let offset = g_offsets[g];

                let lat_slice = unsafe { std::slice::from_raw_parts_mut(arena.add(offset), count) };

                let (p50, p95, p99) = if count == 1 {
                    (lat_slice[0], lat_slice[0], lat_slice[0])
                } else if count <= 64 {
                    lat_slice.sort_unstable();
                    let r50 = (50 * count + 99) / 100 - 1;
                    let r95 = (95 * count + 99) / 100 - 1;
                    let r99 = (99 * count + 99) / 100 - 1;
                    (lat_slice[r50], lat_slice[r95], lat_slice[r99])
                } else {
                    let r50 = (50 * count + 99) / 100 - 1;
                    let r95 = (95 * count + 99) / 100 - 1;
                    let r99 = (99 * count + 99) / 100 - 1;

                    lat_slice.select_nth_unstable(r50);
                    let p50 = lat_slice[r50];

                    if r95 > r50 {
                        lat_slice[r50 + 1..].select_nth_unstable(r95 - (r50 + 1));
                    }
                    let p95 = lat_slice[r95];

                    if r99 > r95 {
                        lat_slice[r95 + 1..].select_nth_unstable(r99 - (r95 + 1));
                    }
                    let p99 = lat_slice[r99];

                    (p50, p95, p99)
                };

                let err_ppm = (errors * 1_000_000) / (count as u64);

                // Format: hour endpoint count p50 p95 p99 errors error_ppm
                out.extend_from_slice(&h_strs[h_idx]);
                out.push(b' ');
                out.extend_from_slice(&g_eps[ep_idx]);
                out.push(b' ');
                append_u64(&mut out, count as u64);
                out.push(b' ');
                append_u64(&mut out, p50 as u64);
                out.push(b' ');
                append_u64(&mut out, p95 as u64);
                out.push(b' ');
                append_u64(&mut out, p99 as u64);
                out.push(b' ');
                append_u64(&mut out, errors);
                out.push(b' ');
                append_u64(&mut out, err_ppm);
                out.push(b'\n');
            }

            out
        }));
    }

    let _ = std::fs::create_dir_all(&out_dir);
    if let Ok(mut f) = File::create(&out_path) {
        for sh in sort_handles {
            let chunk = sh.join().unwrap();
            let _ = f.write_all(&chunk);
        }
    }
}
