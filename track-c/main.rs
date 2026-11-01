// Speed Demon 2026 - Track C (Join) - Ultra High Performance
use std::arch::x86_64::*;
use std::cell::UnsafeCell;
use std::fs::File;
use std::io::Write;
use std::mem::MaybeUninit;
use std::os::unix::io::AsRawFd;
use std::sync::Barrier;
use std::thread;

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn munmap(a: *mut u8, l: usize) -> i32;
    fn madvise(a: *mut u8, l: usize, adv: i32) -> i32;
    fn prctl(opt: i32, a2: u64, a3: u64, a4: u64, a5: u64) -> i32;
    fn _exit(status: i32) -> !;
}

struct SyncCell<T>(UnsafeCell<T>);
unsafe impl<T> Sync for SyncCell<T> {}
unsafe impl<T> Send for SyncCell<T> {}
impl<T> SyncCell<T> {
    fn new(v: T) -> Self { SyncCell(UnsafeCell::new(v)) }
    #[inline(always)] unsafe fn get(&self) -> &mut T { &mut *self.0.get() }
    #[inline(always)] unsafe fn read(&self) -> &T { &*self.0.get() }
}

#[inline(always)]
unsafe fn drop_pages(ptr: *const u8, len: usize) {
    let p = ptr as usize;
    let (s, e) = ((p + 4095) & !4095, (p + len) & !4095);
    if e > s { madvise(s as *mut u8, e - s, 4); }
}

#[inline(always)]
unsafe fn hash_slice(mut p: *const u8, mut len: usize) -> u32 {
    let mut h = len as u64;
    while len >= 8 { h = _mm_crc32_u64(h, (p as *const u64).read_unaligned()); p = p.add(8); len -= 8; }
    if len >= 4 { h = _mm_crc32_u32(h as u32, (p as *const u32).read_unaligned()) as u64; p = p.add(4); len -= 4; }
    while len > 0 { h = _mm_crc32_u8(h as u32, *p) as u64; p = p.add(1); len -= 1; }
    h as u32
}

struct FastMmap { ptr: *const u8, len: usize }
unsafe impl Send for FastMmap {}
unsafe impl Sync for FastMmap {}
impl FastMmap {
    fn open(path: &str, seq: bool) -> Option<Self> {
        let f = File::open(path).ok()?;
        let len = f.metadata().ok()?.len() as usize;
        if len == 0 { return None; }
        let ptr = unsafe { mmap(std::ptr::null_mut(), len, 1, 2, f.as_raw_fd(), 0) };
        if ptr.is_null() || ptr as isize == -1 { return None; }
        if seq { unsafe { madvise(ptr, len, 2); } }
        Some(FastMmap { ptr, len })
    }
    #[inline(always)] fn as_slice(&self) -> &[u8] { unsafe { std::slice::from_raw_parts(self.ptr, self.len) } }
}
impl Drop for FastMmap { fn drop(&mut self) { unsafe { munmap(self.ptr as *mut u8, self.len); } } }

#[inline(always)]
unsafe fn parse_u64_len(p: *const u8, len: usize) -> u64 {
    let mut v = (*p - b'0') as u64;
    for i in 1..len { v = v * 10 + (*p.add(i) - b'0') as u64; }
    v
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
    append_u64(out, pos / 100); out.push(b'.');
    let rem = (pos % 100) as u8;
    out.push(b'0' + rem / 10); out.push(b'0' + rem % 10);
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

const B_CAP: usize = 128;

#[derive(Clone, Copy)]
struct Block { next: u32, hashes: [MaybeUninit<u32>; B_CAP] }

struct ThreadPool {
    heads: [u32; 242],
    lens: [u16; 242],
    blocks: Vec<Block>,
}

impl ThreadPool {
    fn new() -> Self {
        ThreadPool { heads: [!0; 242], lens: [B_CAP as u16; 242], blocks: Vec::with_capacity(2600) }
    }
    #[inline(always)]
    fn push(&mut self, cid: usize, hash: u32) {
        let cur_len = self.lens[cid];
        if cur_len < B_CAP as u16 {
            let h = self.heads[cid] as usize;
            self.blocks[h].hashes[cur_len as usize].write(hash);
            self.lens[cid] = cur_len + 1;
        } else {
            let new_idx = self.blocks.len() as u32;
            let mut blk = Block { next: self.heads[cid], hashes: unsafe { MaybeUninit::uninit().assume_init() } };
            blk.hashes[0].write(hash);
            self.blocks.push(blk);
            self.heads[cid] = new_idx;
            self.lens[cid] = 1;
        }
    }
}

unsafe fn parse_csv_chunk(
    rec_ptr: *const u8, start: usize, end: usize, rlookup: &[u8],
    local_aggs: &mut [LocalCountryAgg], pool: &mut ThreadPool,
) {
    let mut p = rec_ptr.add(start);
    let mut last_drop = p as usize;
    let end_ptr = rec_ptr.add(end);
    let v_quote = _mm256_set1_epi8(b'"' as i8);

    while p < end_ptr {
        if *p == 0xEF && p.add(3) <= end_ptr && *p.add(1) == 0xBB && *p.add(2) == 0xBF { p = p.add(3); }
        if p >= end_ptr { break; }

        let w = (p as *const u64).read_unaligned();
        let v = w ^ 0x2C2C2C2C2C2C2C2Cu64;
        let z = v.wrapping_sub(0x0101010101010101u64) & !v & 0x8080808080808080u64;
        let id_len = (z.trailing_zeros() >> 3) as usize;
        let record_id = parse_u64_len(p, id_len);
        p = p.add(id_len + 1);

        let (name_hash, name_len) = if *p == b',' {
            p = p.add(1); (0u32, 0u64)
        } else if *p == b'"' {
            p = p.add(1);
            let start_name = p;
            let mut q = p;
            while q.add(32) <= end_ptr {
                let mask = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(q as *const __m256i), v_quote)) as u32;
                if mask != 0 { q = q.add(mask.trailing_zeros() as usize); break; }
                q = q.add(32);
            }
            if q.add(32) > end_ptr { while q < end_ptr && *q != b'"' { q = q.add(1); } }
            if q.add(1) < end_ptr && *q.add(1) == b'"' {
                let mut ptr = start_name;
                let (mut h, mut nlen) = (0u64, 0u64);
                while ptr < end_ptr {
                    if *ptr == b'"' {
                        if ptr.add(1) < end_ptr && *ptr.add(1) == b'"' { h = _mm_crc32_u8(h as u32, b'"') as u64; nlen += 1; ptr = ptr.add(2); }
                        else { p = ptr.add(2); break; }
                    } else { h = _mm_crc32_u8(h as u32, *ptr) as u64; nlen += 1; ptr = ptr.add(1); }
                }
                (h as u32, nlen)
            } else {
                let len = q as usize - start_name as usize;
                p = q.add(2);
                (hash_slice(start_name, len), len as u64)
            }
        } else {
            let start_name = p;
            while *p != b',' { p = p.add(1); }
            let len = p as usize - start_name as usize;
            p = p.add(1);
            (hash_slice(start_name, len), len as u64)
        };

        let note_len = if *p == b',' {
            p = p.add(1); 0u64
        } else if *p == b'"' {
            p = p.add(1);
            let start_note = p;
            let mut q = p;
            let mut quote_pairs = 0u64;
            loop {
                while q.add(32) <= end_ptr {
                    let mask = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(q as *const __m256i), v_quote)) as u32;
                    if mask != 0 { q = q.add(mask.trailing_zeros() as usize); break; }
                    q = q.add(32);
                }
                if q.add(32) > end_ptr { while q < end_ptr && *q != b'"' { q = q.add(1); } }
                if q >= end_ptr { p = q; break 0; }
                if q.add(1) < end_ptr && *q.add(1) == b'"' {
                    quote_pairs += 1; q = q.add(2);
                } else {
                    let nlen = (q as usize - start_note as usize) as u64 - quote_pairs;
                    p = q.add(2); break nlen;
                }
            }
        } else {
            let start_note = p;
            while *p != b',' { p = p.add(1); }
            let nlen = p as usize - start_note as usize;
            p = p.add(1);
            nlen as u64
        };

        let w = (p as *const u64).read_unaligned();
        let v = w ^ 0x2C2C2C2C2C2C2C2Cu64;
        let z = v.wrapping_sub(0x0101010101010101u64) & !v & 0x8080808080808080u64;
        let r_len = (z.trailing_zeros() >> 3) as usize;
        let region_id = parse_u64_len(p, r_len) as u32;
        p = p.add(r_len + 1);

        let neg = *p == b'-';
        if neg { p = p.add(1); }
        let w = (p as *const u64).read_unaligned();
        let v = w ^ 0x2E2E2E2E2E2E2E2Eu64;
        let z = v.wrapping_sub(0x0101010101010101u64) & !v & 0x8080808080808080u64;
        let w_len = (z.trailing_zeros() >> 3) as usize;
        let whole = parse_u64_len(p, w_len) as i64;
        p = p.add(w_len + 1);
        let cents_abs = whole * 100 + ((*p - b'0') as i64) * 10 + (*p.add(1) - b'0') as i64;
        let cents = if neg { -cents_abs } else { cents_abs };
        p = p.add(2);
        if *p == b'\r' { p = p.add(2); } else if *p == b'\n' { p = p.add(1); }

        let cid = if (region_id as usize) < rlookup.len() { *rlookup.get_unchecked(region_id as usize) as usize } else { 0 };
        let agg = local_aggs.get_unchecked_mut(cid);
        agg.count += 1;
        agg.id_sum += record_id;
        agg.amount_sum += cents;
        agg.amount_min = agg.amount_min.min(cents);
        agg.amount_max = agg.amount_max.max(cents);
        agg.name_bytes += name_len;
        agg.note_bytes += note_len;

        pool.push(cid, name_hash);

        if (p as usize).saturating_sub(last_drop) >= 2 * 1024 * 1024 {
            drop_pages(last_drop as *const u8, p as usize - last_drop);
            last_drop = p as usize;
        }
    }
    if (end_ptr as usize).saturating_sub(last_drop) > 0 {
        drop_pages(last_drop as *const u8, end_ptr as usize - last_drop);
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

    let r_mmap = match FastMmap::open(&reg_path, false) { Some(m) => m, None => return };
    let r_data = r_mmap.as_slice();
    let mut countries = vec![CountryAgg { key: b"_UNMATCHED".to_vec(), count: 0, id_sum: 0, amount_sum: 0, amount_min: 0, amount_max: 0, name_bytes: 0, note_bytes: 0, distinct_names: 0 }];
    let mut country_lut = [0u8; 17576];
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
        if c_code.len() == 3 {
            let l_idx = ((c_code[0] - b'A') as usize) * 676 + ((c_code[1] - b'A') as usize) * 26 + ((c_code[2] - b'A') as usize);
            let mut cid = country_lut[l_idx];
            if cid == 0 {
                cid = countries.len() as u8;
                country_lut[l_idx] = cid;
                countries.push(CountryAgg { key: c_code.to_vec(), count: 0, id_sum: 0, amount_sum: 0, amount_min: 0, amount_max: 0, name_bytes: 0, note_bytes: 0, distinct_names: 0 });
            }
            if (rid as usize) < flat.len() { flat[rid as usize] = cid; }
        }
        while rp < rend && (r_data[rp] == b'\r' || r_data[rp] == b'\n') { rp += 1; }
    }
    drop(r_mmap);

    let rec_mmap = match FastMmap::open(&rec_path, false) { Some(m) => m, None => return };
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

    let v_cr = unsafe { _mm256_set1_epi8(b'\r' as i8) };
    for t in 1..num_threads {
        let mut pt = data_start + t * (payload_len / num_threads);
        while pt + 32 <= rec_size {
            let chunk = unsafe { _mm256_loadu_si256(rec_data.as_ptr().add(pt) as *const __m256i) };
            let mask = unsafe { _mm256_movemask_epi8(_mm256_cmpeq_epi8(chunk, v_cr)) } as u32;
            if mask != 0 { pt += mask.trailing_zeros() as usize; break; }
            pt += 32;
        }
        while pt < rec_size && rec_data[pt] != b'\r' { pt += 1; }
        let mut next_rec = (pt + 2).min(rec_size);
        if next_rec + 3 <= rec_size && rec_data[next_rec] == 0xEF && rec_data[next_rec + 1] == 0xBB && rec_data[next_rec + 2] == 0xBF { next_rec += 3; }
        splits[t] = next_rec;
    }

    let num_countries = countries.len();
    let mut thread_aggs = vec![vec![LocalCountryAgg::new(); num_countries]; num_threads];
    let thread_pools: Vec<SyncCell<ThreadPool>> = (0..num_threads).map(|_| SyncCell::new(ThreadPool::new())).collect();
    let country_distinct: Vec<SyncCell<usize>> = (0..num_countries).map(|_| SyncCell::new(0)).collect();
    let (b1, b2) = (Barrier::new(num_threads), Barrier::new(num_threads));

    thread::scope(|s| {
        let rec_slice = rec_data;
        let rlookup_ref = &flat;
        let splits_ref = &splits;
        let aggs_slice = &mut thread_aggs[..];
        let pools_ref = &thread_pools;
        let dist_ref = &country_distinct;
        let (b1_ref, b2_ref) = (&b1, &b2);

        for (t, local_aggs) in aggs_slice.iter_mut().enumerate() {
            let (start, end) = (splits_ref[t], splits_ref[t + 1]);
            s.spawn(move || {
                let pool = unsafe { pools_ref[t].get() };
                unsafe {
                    parse_csv_chunk(rec_slice.as_ptr(), start, end, rlookup_ref, local_aggs, pool);
                }
                b1_ref.wait();

                let chunk_size = (num_countries + num_threads - 1) / num_threads;
                let c_start = t * chunk_size;
                let c_end = (c_start + chunk_size).min(num_countries);
                let mut buf = Vec::with_capacity(32768);

                for c in c_start..c_end {
                    buf.clear();
                    for th in 0..num_threads {
                        let pool = unsafe { pools_ref[th].read() };
                        let mut cur = pool.heads[c];
                        let mut first_block = true;
                        while cur != !0 {
                            let blk = &pool.blocks[cur as usize];
                            let len = if first_block { first_block = false; pool.lens[c] as usize } else { B_CAP };
                            let slice: &[u32] = unsafe { std::slice::from_raw_parts(blk.hashes.as_ptr() as *const u32, len) };
                            buf.extend_from_slice(slice);
                            cur = blk.next;
                        }
                    }
                    let d = if buf.is_empty() { 0 } else if buf.len() == 1 { 1 } else {
                        buf.sort_unstable(); buf.dedup(); buf.len()
                    };
                    unsafe { *dist_ref[c].get() = d; }
                }
                b2_ref.wait();
            });
        }
    });

    drop(rec_mmap);

    for c in 0..num_countries {
        let dest = &mut countries[c];
        let mut first = true;
        for t in 0..num_threads {
            let src = &thread_aggs[t][c];
            if src.count > 0 {
                dest.count += src.count; dest.id_sum += src.id_sum; dest.amount_sum += src.amount_sum;
                if first { dest.amount_min = src.amount_min; dest.amount_max = src.amount_max; first = false; }
                else {
                    dest.amount_min = dest.amount_min.min(src.amount_min);
                    dest.amount_max = dest.amount_max.max(src.amount_max);
                }
                dest.name_bytes += src.name_bytes; dest.note_bytes += src.note_bytes;
            }
        }
        dest.distinct_names = unsafe { *country_distinct[c].read() };
    }
    drop(thread_aggs);
    drop(thread_pools);

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
    unsafe { _exit(0); }
}
