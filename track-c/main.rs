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

fn format_cents(cents: i64, out: &mut Vec<u8>) {
    let (sign, pos) = if cents < 0 { (true, (-cents) as u64) } else { (false, cents as u64) };
    if sign { out.push(b'-'); }
    append_u64(out, pos / 100);
    out.push(b'.');
    let rem = (pos % 100) as u8;
    out.push(b'0' + rem / 10);
    out.push(b'0' + rem % 10);
}

#[derive(Clone)]
struct CountryAgg {
    key: Vec<u8>, count: u64, id_sum: u64, amount_sum: i64,
    amount_min: i64, amount_max: i64, name_bytes: u64, note_bytes: u64, distinct_names: usize,
}

#[derive(Clone)]
struct LocalCountryAgg {
    count: u64, id_sum: u64, amount_sum: i64,
    amount_min: i64, amount_max: i64, name_bytes: u64, note_bytes: u64,
}

impl LocalCountryAgg {
    fn new() -> Self {
        LocalCountryAgg { count: 0, id_sum: 0, amount_sum: 0, amount_min: i64::MAX, amount_max: i64::MIN, name_bytes: 0, note_bytes: 0 }
    }
}

fn parse_csv_chunk(rec_data: &[u8], start: usize, end: usize, rlookup: &[u8], local_aggs: &mut [LocalCountryAgg], local_names: &mut Vec<u64>) {
    let mut filter = [0u64; 16384];
    let mut p = start;
    let mut last_evict = start;
    while p < end {
        if p - last_evict >= 2 * 1024 * 1024 {
            unsafe { madvise_dontneed(rec_data.as_ptr().add(last_evict), p - last_evict); }
            last_evict = p;
        }
        if p + 3 <= end && rec_data[p] == 0xEF && rec_data[p + 1] == 0xBB && rec_data[p + 2] == 0xBF { p += 3; }
        if p >= end { break; }

        let mut record_id = 0u64;
        while p < end && rec_data[p] >= b'0' && rec_data[p] <= b'9' { record_id = record_id * 10 + (rec_data[p] - b'0') as u64; p += 1; }
        if p < end && rec_data[p] == b',' { p += 1; }

        let (mut name_hash, mut name_len) = (14695981039346656037u64, 0u64);
        if p < end && rec_data[p] == b'"' {
            p += 1;
            while p < end {
                if rec_data[p] == b'"' {
                    if p + 1 < end && rec_data[p + 1] == b'"' {
                        name_hash = (name_hash ^ (b'"' as u64)).wrapping_mul(1099511628211);
                        name_len += 1; p += 2;
                    } else { p += 1; break; }
                } else {
                    name_hash = (name_hash ^ (rec_data[p] as u64)).wrapping_mul(1099511628211);
                    name_len += 1; p += 1;
                }
            }
        } else {
            while p < end && rec_data[p] != b',' && rec_data[p] != b'\r' && rec_data[p] != b'\n' {
                name_hash = (name_hash ^ (rec_data[p] as u64)).wrapping_mul(1099511628211);
                name_len += 1; p += 1;
            }
        }
        if p < end && rec_data[p] == b',' { p += 1; }

        let mut note_len = 0u64;
        if p < end && rec_data[p] == b'"' {
            p += 1;
            while p < end {
                if rec_data[p] == b'"' {
                    if p + 1 < end && rec_data[p + 1] == b'"' { note_len += 1; p += 2; }
                    else { p += 1; break; }
                } else { note_len += 1; p += 1; }
            }
        } else {
            while p < end && rec_data[p] != b',' && rec_data[p] != b'\r' && rec_data[p] != b'\n' {
                note_len += 1; p += 1;
            }
        }
        if p < end && rec_data[p] == b',' { p += 1; }

        let mut region_id = 0u32;
        while p < end && rec_data[p] >= b'0' && rec_data[p] <= b'9' { region_id = region_id * 10 + (rec_data[p] - b'0') as u32; p += 1; }
        if p < end && rec_data[p] == b',' { p += 1; }

        let mut neg = false;
        if p < end && rec_data[p] == b'-' { neg = true; p += 1; }
        let (mut whole, mut frac) = (0i64, 0i64);
        while p < end && rec_data[p] >= b'0' && rec_data[p] <= b'9' { whole = whole * 10 + (rec_data[p] - b'0') as i64; p += 1; }
        if p < end && rec_data[p] == b'.' { p += 1; }
        if p < end && rec_data[p] >= b'0' && rec_data[p] <= b'9' { frac = frac * 10 + (rec_data[p] - b'0') as i64; p += 1; }
        if p < end && rec_data[p] >= b'0' && rec_data[p] <= b'9' { frac = frac * 10 + (rec_data[p] - b'0') as i64; p += 1; }
        let cents = if neg { -(whole * 100 + frac) } else { whole * 100 + frac };

        while p < end && (rec_data[p] == b'\r' || rec_data[p] == b'\n') { p += 1; }

        let cid = if (region_id as usize) < rlookup.len() { unsafe { *rlookup.get_unchecked(region_id as usize) } } else { 0 };
        let agg = unsafe { local_aggs.get_unchecked_mut(cid as usize) };
        agg.count += 1;
        agg.id_sum += record_id;
        agg.amount_sum += cents;
        if cents < agg.amount_min { agg.amount_min = cents; }
        if cents > agg.amount_max { agg.amount_max = cents; }
        agg.name_bytes += name_len;
        agg.note_bytes += note_len;

        let packed = ((cid as u64) << 48) | (name_hash & 0x0000_FFFF_FFFF_FFFF);
        let slot = (name_hash as usize) & 0x3FFF;
        if filter[slot] != packed {
            filter[slot] = packed;
            local_names.push(packed);
        }
    }
    if end > last_evict { unsafe { madvise_dontneed(rec_data.as_ptr().add(last_evict), end - last_evict); } }
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
    let (rec_path, reg_path, out_path) = (format!("{}/records.csv", data_dir), format!("{}/regions.csv", data_dir), format!("{}/aggregates.txt", out_dir));

    let r_mmap = match FastMmap::open(&reg_path) { Some(m) => m, None => return };
    let r_data = r_mmap.as_slice();
    let mut countries = vec![CountryAgg { key: b"_UNMATCHED".to_vec(), count: 0, id_sum: 0, amount_sum: 0, amount_min: 0, amount_max: 0, name_bytes: 0, note_bytes: 0, distinct_names: 0 }];
    let mut country_map: std::collections::HashMap<Vec<u8>, u8> = std::collections::HashMap::with_capacity(300);
    let (mut rp, rend) = (0, r_data.len());
    while rp < rend && r_data[rp] != b'\n' { rp += 1; }
    if rp < rend && r_data[rp] == b'\n' { rp += 1; }

    let mut flat = vec![0u8; 1_000_000];
    while rp < rend {
        if r_data[rp] == b'\r' || r_data[rp] == b'\n' { rp += 1; continue; }
        let mut rid = 0u32;
        while rp < rend && r_data[rp] >= b'0' && r_data[rp] <= b'9' { rid = rid * 10 + (r_data[rp] - b'0') as u32; rp += 1; }
        if rp < rend && r_data[rp] == b',' { rp += 1; }
        let c_start = rp;
        while rp < rend && r_data[rp] != b'\r' && r_data[rp] != b'\n' { rp += 1; }
        let c_bytes = r_data[c_start..rp].to_vec();
        while rp < rend && (r_data[rp] == b'\r' || r_data[rp] == b'\n') { rp += 1; }

        let next_cid = countries.len() as u8;
        let cid = *country_map.entry(c_bytes.clone()).or_insert_with(|| {
            countries.push(CountryAgg { key: c_bytes, count: 0, id_sum: 0, amount_sum: 0, amount_min: 0, amount_max: 0, name_bytes: 0, note_bytes: 0, distinct_names: 0 });
            next_cid
        });
        if (rid as usize) < flat.len() { flat[rid as usize] = cid; }
        else { flat.resize((rid + 1) as usize, 0); flat[rid as usize] = cid; }
    }
    drop(r_mmap);

    let rec_mmap = match FastMmap::open(&rec_path) { Some(m) => Arc::new(m), None => return };
    let rec_data = rec_mmap.as_slice();
    let rec_size = rec_data.len();
    let mut hdr_p = 0;
    if hdr_p + 3 <= rec_size && rec_data[hdr_p] == 0xEF && rec_data[hdr_p + 1] == 0xBB && rec_data[hdr_p + 2] == 0xBF { hdr_p += 3; }
    while hdr_p < rec_size && rec_data[hdr_p] != b'\n' { hdr_p += 1; }
    if hdr_p < rec_size && rec_data[hdr_p] == b'\n' { hdr_p += 1; }
    let data_start = hdr_p;

    let num_threads = 12.min(thread::available_parallelism().map(|n| n.get()).unwrap_or(12)).max(1);
    let payload_len = rec_size - data_start;
    let mut splits = vec![data_start; num_threads + 1];
    splits[num_threads] = rec_size;

    for t in 1..num_threads {
        let mut pt = data_start + t * (payload_len / num_threads);
        while pt < rec_size {
            if rec_data[pt] == b'\n' {
                let mut probe = pt + 1;
                if probe + 3 <= rec_size && rec_data[probe] == 0xEF && rec_data[probe + 1] == 0xBB && rec_data[probe + 2] == 0xBF { probe += 3; }
                let (mut cur, mut commas) = (probe, 0);
                while cur < rec_size && rec_data[cur] != b'\n' && rec_data[cur] != b'\r' {
                    if rec_data[cur] == b'"' {
                        cur += 1;
                        while cur < rec_size {
                            if rec_data[cur] == b'"' {
                                if cur + 1 < rec_size && rec_data[cur + 1] == b'"' { cur += 2; }
                                else { cur += 1; break; }
                            } else { cur += 1; }
                        }
                    } else {
                        if rec_data[cur] == b',' { commas += 1; }
                        cur += 1;
                    }
                }
                if commas == 4 { pt = probe; break; }
            }
            pt += 1;
        }
        splits[t] = pt.min(rec_size);
    }

    let (num_countries, rlookup_arc) = (countries.len(), Arc::new(flat));
    let mut handles = Vec::with_capacity(num_threads);
    for t in 0..num_threads {
        let (rec_ref, rlookup_ref, start, end) = (Arc::clone(&rec_mmap), Arc::clone(&rlookup_arc), splits[t], splits[t + 1]);
        handles.push(thread::spawn(move || {
            let (mut local_aggs, mut local_names) = (vec![LocalCountryAgg::new(); num_countries], Vec::with_capacity(32768));
            let d = rec_ref.as_slice();
            parse_csv_chunk(d, start, end, &rlookup_ref, &mut local_aggs, &mut local_names);
            unsafe { madvise_dontneed(d.as_ptr().add(start), end - start); }
            (local_aggs, local_names)
        }));
    }

    let mut thread_results = Vec::with_capacity(num_threads);
    let mut all_names: Vec<u64> = Vec::with_capacity(250_000);
    for h in handles {
        let (aggs, names) = h.join().unwrap();
        thread_results.push(aggs);
        all_names.extend_from_slice(&names);
    }
    drop(rec_mmap);

    all_names.sort_unstable();
    all_names.dedup();

    for &packed in &all_names {
        let cid = (packed >> 48) as usize;
        countries[cid].distinct_names += 1;
    }
    drop(all_names);

    for c in 0..num_countries {
        let dest = &mut countries[c];
        for local_aggs in &thread_results {
            let src = &local_aggs[c];
            if src.count > 0 {
                dest.count += src.count; dest.id_sum += src.id_sum; dest.amount_sum += src.amount_sum;
                if src.amount_min < dest.amount_min || dest.count == src.count { dest.amount_min = src.amount_min; }
                if src.amount_max > dest.amount_max || dest.count == src.count { dest.amount_max = src.amount_max; }
                dest.name_bytes += src.name_bytes; dest.note_bytes += src.note_bytes;
            }
        }
    }
    drop(thread_results);

    countries.sort_unstable_by(|a, b| a.key.cmp(&b.key));

    let mut out_buffer = Vec::with_capacity(128 * 1024);
    for c in &countries {
        if c.count == 0 { continue; }
        out_buffer.extend_from_slice(&c.key); out_buffer.push(b' ');
        append_u64(&mut out_buffer, c.count); out_buffer.push(b' ');
        append_u64(&mut out_buffer, c.id_sum); out_buffer.push(b' ');
        format_cents(c.amount_sum, &mut out_buffer); out_buffer.push(b' ');
        format_cents(c.amount_min, &mut out_buffer); out_buffer.push(b' ');
        format_cents(c.amount_max, &mut out_buffer); out_buffer.push(b' ');
        append_u64(&mut out_buffer, c.distinct_names as u64); out_buffer.push(b' ');
        append_u64(&mut out_buffer, c.name_bytes); out_buffer.push(b' ');
        append_u64(&mut out_buffer, c.note_bytes); out_buffer.push(b'\n');
    }
    let _ = std::fs::create_dir_all(&out_dir);
    if let Ok(mut f) = File::create(&out_path) { let _ = f.write_all(&out_buffer); }
}
