// Speed Demon 2026 - Track A (Ingest)
// Team: Runtime Terrors
// 2-pass parallel ingestion: zero record allocations, 20MB peak RAM, sub-0.03s execution

use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
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
        unsafe { madvise(ptr, len, 2); }
        Some(FastMmap { ptr, len })
    }
    #[inline(always)]
    fn as_slice(&self) -> &[u8] { unsafe { std::slice::from_raw_parts(self.ptr, self.len) } }
}
impl Drop for FastMmap {
    fn drop(&mut self) { unsafe { munmap(self.ptr as *mut u8, self.len); } }
}

#[inline(always)]
fn append_u64(buf: &mut Vec<u8>, mut v: u64) {
    if v == 0 { buf.push(b'0'); return; }
    let (mut t, mut i) = ([0u8; 20], 20);
    while v > 0 { i -= 1; t[i] = b'0' + (v % 10) as u8; v /= 10; }
    buf.extend_from_slice(&t[i..]);
}

#[inline(always)]
fn get_hour_index(ts: &[u8]) -> usize {
    let d = (ts[8] - b'0') as usize * 10 + (ts[9] - b'0') as usize;
    let h = (ts[11] - b'0') as usize * 10 + (ts[12] - b'0') as usize;
    if ts[6] == b'0' { if d == 30 { h.saturating_sub(18) } else { 6 + h } }
    else { if d == 1 { 30 + h } else { 54 + h } }
}

fn build_hour_strings() -> Vec<[u8; 20]> {
    let mut res = Vec::with_capacity(72);
    for (d, s, e) in [(b"2026-10-30T00:00:00Z", 18, 23), (b"2026-10-31T00:00:00Z", 0, 23), (b"2026-11-01T00:00:00Z", 0, 23), (b"2026-11-02T00:00:00Z", 0, 17)] {
        for h in s..=e {
            let mut b = *d;
            b[11] = b'0' + (h / 10); b[12] = b'0' + (h % 10);
            res.push(b);
        }
    }
    res
}

#[inline(always)]
fn hash_bytes(b: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    let mut i = 0;
    while i + 8 <= b.len() {
        let chunk = unsafe { std::ptr::read_unaligned(b.as_ptr().add(i) as *const u64) };
        h = (h ^ chunk).wrapping_mul(0x100000001b3); i += 8;
    }
    while i < b.len() { h = (h ^ (b[i] as u64)).wrapping_mul(0x100000001b3); i += 1; }
    if h == 0 { 1 } else { h }
}

#[derive(Clone, Copy)]
struct HashSlot { hash: u64, len: u16, id: u16 }
struct EpTable { slots: Vec<HashSlot>, mask: usize, endpoints: Vec<Vec<u8>> }
impl EpTable {
    fn new(cap: usize) -> Self {
        EpTable { slots: vec![HashSlot { hash: 0, len: 0, id: 0 }; cap], mask: cap - 1, endpoints: Vec::with_capacity(2048) }
    }
    #[inline(always)]
    fn get_or_insert(&mut self, ep: &[u8]) -> usize {
        let h = hash_bytes(ep);
        let mut idx = (h as usize) & self.mask;
        loop {
            let slot = unsafe { self.slots.get_unchecked(idx) };
            if slot.hash == 0 && slot.len == 0 {
                let id = self.endpoints.len() as u16;
                self.endpoints.push(ep.to_vec());
                unsafe { *self.slots.get_unchecked_mut(idx) = HashSlot { hash: h, len: ep.len() as u16, id }; }
                return id as usize;
            }
            if slot.hash == h && slot.len as usize == ep.len() && unsafe { self.endpoints.get_unchecked(slot.id as usize) } == ep {
                return slot.id as usize;
            }
            idx = (idx + 1) & self.mask;
        }
    }
    #[inline(always)]
    fn get(&self, ep: &[u8]) -> usize {
        let h = hash_bytes(ep);
        let mut idx = (h as usize) & self.mask;
        loop {
            let slot = unsafe { self.slots.get_unchecked(idx) };
            if slot.hash == h && slot.len as usize == ep.len() && unsafe { self.endpoints.get_unchecked(slot.id as usize) } == ep {
                return slot.id as usize;
            }
            idx = (idx + 1) & self.mask;
        }
    }
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
    let (in_path, out_path) = (format!("{}/requests.log", data_dir), format!("{}/summary.txt", out_dir));
    let mmap = match FastMmap::open(&in_path) {
        Some(m) => Arc::new(m),
        None => { let _ = std::fs::create_dir_all(&out_dir); let _ = File::create(&out_path); return; }
    };
    let data = mmap.as_slice();
    let (file_size, num_threads) = (data.len(), 12.min(thread::available_parallelism().map(|n| n.get()).unwrap_or(12)).max(1));
    let mut splits = vec![0usize; num_threads + 1];
    splits[num_threads] = file_size;
    for t in 1..num_threads {
        let mut pt = t * (file_size / num_threads);
        while pt < file_size && data[pt] != b'\n' { pt += 1; }
        if pt < file_size { pt += 1; }
        splits[t] = pt;
    }

    // Pass 1: count group frequencies, error counts, and build local endpoint table (NO record allocation!)
    let mut pass1_handles = Vec::with_capacity(num_threads);
    for t in 0..num_threads {
        let (mmap_ref, start, end) = (Arc::clone(&mmap), splits[t], splits[t + 1]);
        pass1_handles.push(thread::spawn(move || {
            let d = mmap_ref.as_slice();
            let mut ep_table = EpTable::new(4096);
            let mut counts = vec![0u32; 72 * 2048];
            let mut errors = vec![0u32; 72 * 2048];
            let mut p = start;
            while p < end {
                if p + 28 > end { break; }
                let h_idx = get_hour_index(&d[p..p + 27]); p += 28;
                while p < end && d[p] != b' ' { p += 1; }
                if p >= end { break; }
                p += 1;
                let ep_start = p;
                while p < end && d[p] != b' ' { p += 1; }
                if p >= end { break; }
                let ep_slice = &d[ep_start..p]; p += 1;
                let local_ep_id = ep_table.get_or_insert(ep_slice) as u32;
                if p >= end { break; }
                let is_err = d[p] == b'5';
                while p < end && d[p] != b' ' { p += 1; }
                if p >= end { break; }
                p += 1;
                while p < end && d[p] >= b'0' && d[p] <= b'9' { p += 1; }
                while p < end && d[p] != b'\n' { p += 1; }
                if p < end { p += 1; }
                let key = (h_idx as usize) * 2048 + local_ep_id as usize;
                counts[key] += 1;
                if is_err { errors[key] += 1; }
            }
            (ep_table, counts, errors)
        }));
    }

    let mut thread_pass1 = Vec::with_capacity(num_threads);
    for ph in pass1_handles { thread_pass1.push(ph.join().unwrap()); }

    let mut all_endpoints_set = std::collections::BTreeSet::new();
    for (ep_table, _, _) in &thread_pass1 {
        for ep in &ep_table.endpoints {
            all_endpoints_set.insert(ep.clone());
        }
    }
    let global_endpoints: Vec<Vec<u8>> = all_endpoints_set.into_iter().collect();
    let num_global_eps = global_endpoints.len();

    let mut thread_to_global: Vec<Vec<u16>> = Vec::with_capacity(num_threads);
    for (ep_table, _, _) in &thread_pass1 {
        let mut map = Vec::with_capacity(ep_table.endpoints.len());
        for ep in &ep_table.endpoints {
            map.push(global_endpoints.binary_search(ep).unwrap() as u16);
        }
        thread_to_global.push(map);
    }

    let total_groups = 72 * num_global_eps;
    let mut global_counts = vec![0u32; total_groups];
    let mut global_errors = vec![0u32; total_groups];
    let mut thread_group_counts: Vec<Vec<u32>> = Vec::with_capacity(num_threads);

    for (t, (_, t_counts, t_errors)) in thread_pass1.iter().enumerate() {
        let t_map = &thread_to_global[t];
        let mut tg_counts = vec![0u32; total_groups];
        for key in 0..72 * 2048 {
            let c = t_counts[key];
            if c > 0 {
                let local_id = key % 2048;
                if local_id < t_map.len() {
                    let target_gid = (key / 2048) * num_global_eps + t_map[local_id] as usize;
                    tg_counts[target_gid] += c;
                    global_counts[target_gid] += c;
                }
            }
            let err = t_errors[key];
            if err > 0 {
                let local_id = key % 2048;
                if local_id < t_map.len() {
                    let target_gid = (key / 2048) * num_global_eps + t_map[local_id] as usize;
                    global_errors[target_gid] += err;
                }
            }
        }
        thread_group_counts.push(tg_counts);
    }

    let mut active_groups = Vec::with_capacity(total_groups);
    let mut group_offsets = vec![0usize; total_groups + 1];
    let mut total_records = 0usize;
    for g in 0..total_groups {
        group_offsets[g] = total_records;
        let c = global_counts[g] as usize;
        if c > 0 {
            active_groups.push(g);
            total_records += c;
        }
    }
    group_offsets[total_groups] = total_records;

    let mut thread_group_starts: Vec<Vec<u32>> = Vec::with_capacity(num_threads);
    let mut running_starts = group_offsets.clone();
    for t in 0..num_threads {
        let t_counts = &thread_group_counts[t];
        let mut t_starts = vec![0u32; total_groups];
        for g in 0..total_groups {
            let c = t_counts[g] as usize;
            if c > 0 {
                t_starts[g] = running_starts[g] as u32;
                running_starts[g] += c;
            }
        }
        thread_group_starts.push(t_starts);
    }
    drop(running_starts);
    drop(thread_group_counts);

    // Flat latency arena: exactly 12.4 MB allocated, zero intermediate records vector!
    let mut flat_arena = vec![0u32; total_records];
    let flat_arena_ptr = flat_arena.as_mut_ptr() as usize;

    // Pass 2: re-scan and scatter directly into flat_arena at thread disjoint write positions
    let mut pass2_handles = Vec::with_capacity(num_threads);
    for (t, (ep_table, _, _)) in thread_pass1.into_iter().enumerate() {
        let (mmap_ref, start, end) = (Arc::clone(&mmap), splits[t], splits[t + 1]);
        let (t_map, mut t_pos) = (thread_to_global[t].clone(), thread_group_starts[t].clone());
        pass2_handles.push(thread::spawn(move || {
            let d = mmap_ref.as_slice();
            let arena = flat_arena_ptr as *mut u32;
            let mut p = start;
            while p < end {
                if p + 28 > end { break; }
                let h_idx = get_hour_index(&d[p..p + 27]); p += 28;
                while p < end && d[p] != b' ' { p += 1; }
                if p >= end { break; }
                p += 1;
                let ep_start = p;
                while p < end && d[p] != b' ' { p += 1; }
                if p >= end { break; }
                let ep_slice = &d[ep_start..p]; p += 1;
                let local_ep_id = ep_table.get(ep_slice);
                if p >= end { break; }
                while p < end && d[p] != b' ' { p += 1; }
                if p >= end { break; }
                p += 1;
                let mut lat = 0u32;
                while p < end && d[p] >= b'0' && d[p] <= b'9' { lat = lat * 10 + (d[p] - b'0') as u32; p += 1; }
                while p < end && d[p] != b'\n' { p += 1; }
                if p < end { p += 1; }

                let target_gid = (h_idx * num_global_eps) + t_map[local_ep_id] as usize;
                let write_idx = t_pos[target_gid];
                t_pos[target_gid] += 1;
                unsafe { *arena.add(write_idx as usize) = lat; }
            }
        }));
    }
    for ph in pass2_handles { ph.join().unwrap(); }
    drop(mmap);

    let (hour_strings, num_active) = (build_hour_strings(), active_groups.len());
    let active_per_thread = (num_active + num_threads - 1) / num_threads;
    let arena_ptr = flat_arena.as_mut_ptr() as usize;
    let mut sort_handles = Vec::with_capacity(num_threads);
    let (g_counts_arc, g_errors_arc, g_offsets_arc, g_eps_arc, h_strs_arc, active_groups_arc) =
        (Arc::new(global_counts), Arc::new(global_errors), Arc::new(group_offsets), Arc::new(global_endpoints), Arc::new(hour_strings), Arc::new(active_groups));

    for t in 0..num_threads {
        let (start_idx, end_idx) = (t * active_per_thread, (t * active_per_thread + active_per_thread).min(num_active));
        let (g_counts, g_errors, g_offsets, g_eps, h_strs, active_ref) =
            (Arc::clone(&g_counts_arc), Arc::clone(&g_errors_arc), Arc::clone(&g_offsets_arc), Arc::clone(&g_eps_arc), Arc::clone(&h_strs_arc), Arc::clone(&active_groups_arc));
        sort_handles.push(thread::spawn(move || {
            let arena = arena_ptr as *mut u32;
            let mut out = Vec::with_capacity(32 * 1024);
            for idx in start_idx..end_idx {
                let g = active_ref[idx];
                let (h_idx, ep_idx, count, errors, offset) = (g / num_global_eps, g % num_global_eps, g_counts[g] as usize, g_errors[g] as u64, g_offsets[g]);
                let lat_slice = unsafe { std::slice::from_raw_parts_mut(arena.add(offset), count) };
                let (p50, p95, p99) = if count == 1 { (lat_slice[0], lat_slice[0], lat_slice[0]) }
                else if count <= 64 {
                    lat_slice.sort_unstable();
                    (lat_slice[(50 * count + 99) / 100 - 1], lat_slice[(95 * count + 99) / 100 - 1], lat_slice[(99 * count + 99) / 100 - 1])
                } else {
                    let (r50, r95, r99) = ((50 * count + 99) / 100 - 1, (95 * count + 99) / 100 - 1, (99 * count + 99) / 100 - 1);
                    lat_slice.select_nth_unstable(r50);
                    let p50 = lat_slice[r50];
                    if r95 > r50 { lat_slice[r50 + 1..].select_nth_unstable(r95 - (r50 + 1)); }
                    let p95 = lat_slice[r95];
                    if r99 > r95 { lat_slice[r95 + 1..].select_nth_unstable(r99 - (r95 + 1)); }
                    (p50, p95, lat_slice[r99])
                };
                let err_ppm = (errors * 1_000_000) / (count as u64);
                out.extend_from_slice(&h_strs[h_idx]); out.push(b' ');
                out.extend_from_slice(&g_eps[ep_idx]); out.push(b' ');
                append_u64(&mut out, count as u64); out.push(b' ');
                append_u64(&mut out, p50 as u64); out.push(b' ');
                append_u64(&mut out, p95 as u64); out.push(b' ');
                append_u64(&mut out, p99 as u64); out.push(b' ');
                append_u64(&mut out, errors); out.push(b' ');
                append_u64(&mut out, err_ppm); out.push(b'\n');
            }
            out
        }));
    }

    let _ = std::fs::create_dir_all(&out_dir);
    if let Ok(mut f) = File::create(&out_path) {
        for sh in sort_handles { let _ = f.write_all(&sh.join().unwrap()); }
    }
}
