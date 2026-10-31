// Speed Demon 2026 - Track C (Join) - Runtime Terrors
use std::arch::x86_64::*;
use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::thread;

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn munmap(a: *mut u8, l: usize) -> i32;
    fn madvise(a: *mut u8, l: usize, adv: i32) -> i32;
    fn prctl(opt: i32, a2: u64, a3: u64, a4: u64, a5: u64) -> i32;
    fn _exit(status: i32) -> !;
}

#[inline(always)]
unsafe fn madvise_dontneed(ptr: *const u8, len: usize) {
    let p = ptr as usize;
    let (s, e) = ((p + 4095) & !4095, (p + len) & !4095);
    if e > s { madvise(s as *mut u8, e - s, 4); }
}

#[inline(always)]
unsafe fn find_comma(mut p: *const u8, end_ptr: *const u8) -> *const u8 {
    let v = _mm256_set1_epi8(b',' as i8);
    while p.add(32) <= end_ptr {
        let chunk = _mm256_loadu_si256(p as *const __m256i);
        let mask = _mm256_movemask_epi8(_mm256_cmpeq_epi8(chunk, v)) as u32;
        if mask != 0 { return p.add(mask.trailing_zeros() as usize); }
        p = p.add(32);
    }
    while p < end_ptr && *p != b',' { p = p.add(1); }
    p
}

#[inline(always)]
unsafe fn find_quote(mut p: *const u8, end_ptr: *const u8) -> *const u8 {
    let v = _mm256_set1_epi8(b'"' as i8);
    while p.add(32) <= end_ptr {
        let chunk = _mm256_loadu_si256(p as *const __m256i);
        let mask = _mm256_movemask_epi8(_mm256_cmpeq_epi8(chunk, v)) as u32;
        if mask != 0 { return p.add(mask.trailing_zeros() as usize); }
        p = p.add(32);
    }
    while p < end_ptr && *p != b'"' { p = p.add(1); }
    p
}

#[inline(always)]
unsafe fn hash_slice(mut p: *const u8, len: usize) -> u32 {
    let mut h = len as u64;
    let mut rem = len;
    while rem >= 8 {
        h = _mm_crc32_u64(h, (p as *const u64).read_unaligned());
        p = p.add(8); rem -= 8;
    }
    if rem >= 4 {
        h = _mm_crc32_u32(h as u32, (p as *const u32).read_unaligned()) as u64;
        p = p.add(4); rem -= 4;
    }
    while rem > 0 {
        h = _mm_crc32_u8(h as u32, *p) as u64;
        p = p.add(1); rem -= 1;
    }
    h as u32
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
        if ptr.is_null() || ptr as isize == -1 { return None; }
        unsafe { madvise(ptr, len, 3 | 2); }
        Some(FastMmap { ptr, len })
    }
    #[inline(always)] fn as_slice(&self) -> &[u8] { unsafe { std::slice::from_raw_parts(self.ptr, self.len) } }
}
impl Drop for FastMmap { fn drop(&mut self) { unsafe { munmap(self.ptr as *mut u8, self.len); } } }

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

unsafe fn parse_csv_chunk(
    rec_ptr: *const u8, start: usize, end: usize, rlookup: &[u8],
    local_aggs: &mut [LocalCountryAgg], filter: &mut [[u64; 2]], local_names: &mut Vec<u64>,
) {
    let mut p = rec_ptr.add(start);
    let end_ptr = rec_ptr.add(end);
    while p < end_ptr {
        if p.add(3) <= end_ptr && *p == 0xEF && *p.add(1) == 0xBB && *p.add(2) == 0xBF { p = p.add(3); }
        if p >= end_ptr { break; }

        let mut record_id = 0u64;
        while p < end_ptr && *p >= b'0' && *p <= b'9' { record_id = record_id * 10 + (*p - b'0') as u64; p = p.add(1); }
        if p < end_ptr && *p == b',' { p = p.add(1); }

        let (name_hash, name_len);
        if p < end_ptr && *p == b'"' {
            p = p.add(1);
            let start_name = p;
            p = find_quote(p, end_ptr);
            if p.add(1) < end_ptr && *p.add(1) == b'"' {
                let mut ptr = start_name;
                let (mut h, mut nlen) = (0u64, 0u64);
                while ptr < end_ptr {
                    if *ptr == b'"' {
                        if ptr.add(1) < end_ptr && *ptr.add(1) == b'"' {
                            h = _mm_crc32_u8(h as u32, b'"') as u64;
                            nlen += 1; ptr = ptr.add(2);
                        } else { p = ptr.add(1); break; }
                    } else {
                        h = _mm_crc32_u8(h as u32, *ptr) as u64;
                        nlen += 1; ptr = ptr.add(1);
                    }
                }
                name_hash = h as u32; name_len = nlen;
            } else {
                let len = p as usize - start_name as usize;
                name_hash = hash_slice(start_name, len);
                name_len = len as u64;
                if p < end_ptr { p = p.add(1); }
            }
        } else {
            let start_name = p;
            p = find_comma(p, end_ptr);
            let len = p as usize - start_name as usize;
            name_hash = hash_slice(start_name, len);
            name_len = len as u64;
        }
        if p < end_ptr && *p == b',' { p = p.add(1); }

        let mut note_len = 0u64;
        if p < end_ptr && *p == b'"' {
            p = p.add(1);
            let start_note = p;
            p = find_quote(p, end_ptr);
            if p.add(1) < end_ptr && *p.add(1) == b'"' {
                let mut ptr = start_note;
                let mut quote_pairs = 0u64;
                while ptr < end_ptr {
                    if *ptr == b'"' {
                        if ptr.add(1) < end_ptr && *ptr.add(1) == b'"' { quote_pairs += 1; ptr = ptr.add(2); }
                        else { note_len = (ptr as usize - start_note as usize) as u64 - quote_pairs; p = ptr.add(1); break; }
                    } else { ptr = ptr.add(1); }
                }
            } else {
                note_len = (p as usize - start_note as usize) as u64;
                if p < end_ptr { p = p.add(1); }
            }
        } else {
            let start_note = p;
            p = find_comma(p, end_ptr);
            note_len = (p as usize - start_note as usize) as u64;
        }
        if p < end_ptr && *p == b',' { p = p.add(1); }

        let mut region_id = 0u32;
        while p < end_ptr && *p >= b'0' && *p <= b'9' { region_id = region_id * 10 + (*p - b'0') as u32; p = p.add(1); }
        if p < end_ptr && *p == b',' { p = p.add(1); }

        let mut neg = false;
        if p < end_ptr && *p == b'-' { neg = true; p = p.add(1); }
        let mut whole = 0i64;
        while p < end_ptr && *p >= b'0' && *p <= b'9' { whole = whole * 10 + (*p - b'0') as i64; p = p.add(1); }
        let mut frac = 0i64;
        if p < end_ptr && *p == b'.' {
            p = p.add(1);
            if p.add(2) <= end_ptr {
                frac = ((*p - b'0') as i64) * 10 + (*p.add(1) - b'0') as i64;
                p = p.add(2);
            }
        }
        let cents = if neg { -(whole * 100 + frac) } else { whole * 100 + frac };
        while p < end_ptr && (*p == b'\r' || *p == b'\n') { p = p.add(1); }

        let cid = if (region_id as usize) < rlookup.len() { *rlookup.get_unchecked(region_id as usize) as usize } else { 0 };
        let agg = local_aggs.get_unchecked_mut(cid);
        agg.count += 1;
        agg.id_sum += record_id;
        agg.amount_sum += cents;
        if cents < agg.amount_min { agg.amount_min = cents; }
        if cents > agg.amount_max { agg.amount_max = cents; }
        agg.name_bytes += name_len;
        agg.note_bytes += note_len;

        let packed = (((cid as u64) + 1) << 48) | (name_hash as u64);
        let slot = (name_hash as usize) & 16383;
        let row = filter.get_unchecked_mut(slot);
        if row[0] != packed && row[1] != packed {
            row[1] = row[0];
            row[0] = packed;
            local_names.push(packed);
        }
    }
}

fn main() {
    unsafe { prctl(14, 1, 0, 0, 0); }
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
        let c_code = &r_data[c_start..rp];
        let cid = match country_map.get(c_code) {
            Some(&id) => id,
            None => {
                let id = countries.len() as u8;
                country_map.insert(c_code.to_vec(), id);
                countries.push(CountryAgg { key: c_code.to_vec(), count: 0, id_sum: 0, amount_sum: 0, amount_min: 0, amount_max: 0, name_bytes: 0, note_bytes: 0, distinct_names: 0 });
                id
            }
        };
        if (rid as usize) < flat.len() { flat[rid as usize] = cid; }
        while rp < rend && (r_data[rp] == b'\r' || r_data[rp] == b'\n') { rp += 1; }
    }
    drop(r_mmap);

    let rec_mmap = match FastMmap::open(&rec_path) { Some(m) => m, None => return };
    let rec_data = rec_mmap.as_slice();
    let rec_size = rec_data.len();

    let mut data_start = 0;
    if rec_size >= 3 && rec_data[0] == 0xEF && rec_data[1] == 0xBB && rec_data[2] == 0xBF { data_start = 3; }
    while data_start < rec_size && rec_data[data_start] != b'\n' { data_start += 1; }
    if data_start < rec_size && rec_data[data_start] == b'\n' { data_start += 1; }

    let num_threads = 12;
    let payload_len = rec_size.saturating_sub(data_start);
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

    let num_countries = countries.len();
    let mut thread_aggs = vec![vec![LocalCountryAgg::new(); num_countries]; num_threads];
    let mut thread_names = vec![Vec::<u64>::new(); num_threads];

    thread::scope(|s| {
        let rec_slice = rec_data;
        let rlookup_ref = &flat;
        let splits_ref = &splits;
        let aggs_slice = &mut thread_aggs[..];
        let names_slice = &mut thread_names[..];

        for (t, (local_aggs, local_names)) in aggs_slice.iter_mut().zip(names_slice.iter_mut()).enumerate() {
            let (start, end) = (splits_ref[t], splits_ref[t + 1]);
            s.spawn(move || {
                let mut filter = vec![[0u64; 2]; 16384];
                local_names.reserve(32768);
                unsafe {
                    parse_csv_chunk(rec_slice.as_ptr(), start, end, rlookup_ref, local_aggs, &mut filter, local_names);
                }
                drop(filter);
                local_names.sort_unstable();
                local_names.dedup();
                local_names.shrink_to_fit();
            });
        }
    });

    unsafe { madvise_dontneed(rec_data.as_ptr(), rec_size); }
    drop(rec_mmap);

    let mut thread_offsets = Vec::with_capacity(num_threads);
    for t in 0..num_threads {
        let names = &thread_names[t];
        let mut offsets = vec![0usize; num_countries + 1];
        let mut curr_cid = 0usize;
        for (idx, &packed) in names.iter().enumerate() {
            let cid = ((packed >> 48) as usize).saturating_sub(1);
            while curr_cid < cid {
                curr_cid += 1;
                offsets[curr_cid] = idx;
            }
        }
        while curr_cid < num_countries {
            curr_cid += 1;
            offsets[curr_cid] = names.len();
        }
        thread_offsets.push(offsets);
    }

    let mut buf = Vec::with_capacity(4096);
    for c in 0..num_countries {
        let dest = &mut countries[c];
        let mut first = true;
        buf.clear();
        for t in 0..num_threads {
            let src = &thread_aggs[t][c];
            if src.count > 0 {
                dest.count += src.count;
                dest.id_sum += src.id_sum;
                dest.amount_sum += src.amount_sum;
                if first {
                    dest.amount_min = src.amount_min; dest.amount_max = src.amount_max; first = false;
                } else {
                    if src.amount_min < dest.amount_min { dest.amount_min = src.amount_min; }
                    if src.amount_max > dest.amount_max { dest.amount_max = src.amount_max; }
                }
                dest.name_bytes += src.name_bytes;
                dest.note_bytes += src.note_bytes;
            }
            let s_idx = thread_offsets[t][c];
            let e_idx = thread_offsets[t][c + 1];
            if e_idx > s_idx {
                buf.extend_from_slice(&thread_names[t][s_idx..e_idx]);
            }
        }
        dest.distinct_names = if buf.is_empty() { 0 } else if buf.len() == 1 { 1 } else {
            buf.sort_unstable();
            buf.dedup();
            buf.len()
        };
    }
    drop(thread_aggs);
    drop(thread_names);

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
    if let Ok(mut f) = File::create(&out_path) {
        let _ = f.write_all(&out_buffer);
    }
    unsafe { _exit(0); }
}
