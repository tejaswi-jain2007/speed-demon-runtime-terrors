// Speed Demon 2026 - Track A (Ingest) - Runtime Terrors
use std::arch::x86_64::*;
use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    fn prctl(option: i32, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> i32;
    fn _exit(status: i32) -> !;
}

const PROT_READ: i32 = 1; const PROT_WRITE: i32 = 2; const MAP_PRIVATE: i32 = 0x02; const MAP_ANONYMOUS: i32 = 0x20;
const MAP_FIXED: i32 = 0x10; const MAP_FAILED: *mut u8 = !0 as *mut u8; const MADV_DONTNEED: i32 = 4; const MADV_SEQUENTIAL: i32 = 2;
const HOURS: usize = 72; const KEY_BITS: u32 = 12; const ID_MASK: u32 = (1 << 11) - 1; const ERR_BIT: u32 = 1 << 11;
const BIG: u32 = 1 << 20; const NTHREADS: usize = 12; const BLOCK_CAP: usize = (16384 - 16) / 4; const TABLE_SLOTS: usize = 2048;
const BLOCKS_PER_WORKER: usize = 128;

#[inline(always)] unsafe fn load8(p: *const u8) -> u64 { (p as *const u64).read_unaligned() }

#[inline(always)]
unsafe fn find(mut s: *const u8, c: u8) -> *const u8 {
    let target = _mm256_set1_epi8(c as i8);
    loop {
        let v = _mm256_loadu_si256(s as *const __m256i);
        let m = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, target)) as u32;
        if m != 0 { return s.add(m.trailing_zeros() as usize); }
        s = s.add(32);
    }
}

#[inline(always)]
unsafe fn parse_digits(end: *const u8, len: u32) -> u32 {
    let v = _mm_loadu_si128(end.sub(16) as *const __m128i);
    let v = _mm_subs_epu8(v, _mm_set1_epi8(b'0' as i8));
    let lane = _mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    let v = _mm_and_si128(v, _mm_cmpgt_epi8(lane, _mm_set1_epi8((15 - len) as i8)));
    let v = _mm_maddubs_epi16(v, _mm_set1_epi16(0x010A));
    let v = _mm_madd_epi16(v, _mm_set1_epi32(0x00010064));
    let v = _mm_packus_epi32(v, v);
    let v = _mm_madd_epi16(v, _mm_set1_epi32(0x00012710));
    let (h, l) = (_mm_cvtsi128_si32(v) as u32, _mm_extract_epi32(v, 1) as u32);
    h.wrapping_mul(100000000) + l
}

#[derive(Copy, Clone)] struct SendPtr<T>(*const T);
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}
impl<T> SendPtr<T> { #[inline(always)] fn as_ptr(&self) -> *const T { self.0 } }

#[derive(Copy, Clone, Default)]
struct Slot { head8: u64, tail8: u64, ptr: *const u8, len: u32, id: u32 }

struct EndpointTable<'a> { slots: [Slot; TABLE_SLOTS], names: Vec<&'a [u8]> }

impl<'a> EndpointTable<'a> {
    fn new() -> Self { Self { slots: [Slot::default(); TABLE_SLOTS], names: Vec::with_capacity(2048) } }

    #[inline(always)]
    unsafe fn intern(&mut self, s: *const u8, len: usize) -> u32 {
        let mut a = load8(s);
        let b = if len >= 8 { load8(s.add(len - 8)) } else { 0 };
        if len < 8 { a &= (1u64 << (8 * len)) - 1; }
        let h1 = _mm_crc32_u64(len as u64, a);
        let mut i = _mm_crc32_u64(h1, b) as usize;
        loop {
            let slot = &mut self.slots[i & (TABLE_SLOTS - 1)];
            if slot.id == 0 {
                let idx = self.names.len() as u32;
                self.names.push(std::slice::from_raw_parts(s, len));
                *slot = Slot { head8: a, tail8: b, ptr: s, len: len as u32, id: idx + 1 };
                return idx;
            }
            if slot.len == len as u32 && slot.head8 == a && slot.tail8 == b {
                if len <= 16 || std::slice::from_raw_parts(s, len) == std::slice::from_raw_parts(slot.ptr, len) {
                    return slot.id - 1;
                }
            }
            i += 1;
        }
    }
}

#[repr(C)]
struct Block { next: *mut Block, n: u32, _pad: u32, rec: [u32; BLOCK_CAP] }

struct Worker<'a> {
    endpoints: EndpointTable<'a>,
    head: [*mut Block; HOURS],
    big: Vec<Vec<(u32, u32)>>,
    pool_base: *mut Block,
    pool_cap: usize,
    pool_used: usize,
}
unsafe impl<'a> Send for Worker<'a> {}
unsafe impl<'a> Sync for Worker<'a> {}

impl<'a> Worker<'a> {
    fn new() -> Self {
        Self {
            endpoints: EndpointTable::new(),
            head: [std::ptr::null_mut(); HOURS],
            big: (0..HOURS).map(|_| Vec::new()).collect(),
            pool_base: std::ptr::null_mut(),
            pool_cap: 0,
            pool_used: 0,
        }
    }

    #[inline(always)]
    unsafe fn alloc_block(&mut self) -> *mut Block {
        if self.pool_used >= self.pool_cap {
            let extra = 16 * std::mem::size_of::<Block>();
            let p = mmap(std::ptr::null_mut(), extra, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            if p == MAP_FAILED { _exit(1); }
            self.pool_base = p as *mut Block; self.pool_cap = 16; self.pool_used = 0;
        }
        let b = self.pool_base.add(self.pool_used);
        self.pool_used += 1;
        (*b).next = std::ptr::null_mut();
        (*b).n = 0;
        b
    }

    unsafe fn parse(&mut self, mut p: *const u8, end: *const u8) {
        if p >= end { return; }
        let space_vec = _mm256_set1_epi8(b' ' as i8);
        let nl_vec = _mm256_set1_epi8(b'\n' as i8);

        while p < end {
            let day_idx = if *p.add(6) == b'0' { (*p.add(9) - b'0') as i32 } else { (*p.add(9) - b'0' + 1) as i32 };
            let hour = ((*p.add(11) - b'0') * 10 + (*p.add(12) - b'0')) as i32;
            let h = (day_idx * 24 + hour - 18) as usize;

            let m = p.add(28);
            let v0 = _mm256_loadu_si256(m as *const __m256i);
            let v1 = _mm256_loadu_si256(m.add(32) as *const __m256i);
            let sp0 = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v0, space_vec)) as u32;
            let sp1 = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v1, space_vec)) as u32;
            let nl0 = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v0, nl_vec)) as u32;
            let nl1 = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v1, nl_vec)) as u32;

            let sp = (sp0 as u64) | ((sp1 as u64) << 32);
            let nl = (nl0 as u64) | ((nl1 as u64) << 32);

            let ep = m.add(sp.trailing_zeros() as usize + 1);
            let ee = if nl != 0 { m.add((sp & (sp - 1)).trailing_zeros() as usize) } else { find(ep, b' ') };
            let eol = if nl != 0 { m.add(nl.trailing_zeros() as usize) } else { find(ee.add(5), b'\n') };

            let is_err = if *ee.add(1) == b'5' { ERR_BIT } else { 0 };
            let key = self.endpoints.intern(ep, ee as usize - ep as usize) | is_err;
            let lat = parse_digits(eol, (eol as usize - ee as usize - 5) as u32);
            p = eol.add(1);

            if lat >= BIG { self.big[h].push((lat, key)); continue; }

            let b = &mut self.head[h];
            if (*b).is_null() || (*(*b)).n as usize == BLOCK_CAP {
                let fresh = self.alloc_block();
                (*fresh).next = *b;
                *b = fresh;
            }
            let b_ref = &mut *(*b);
            b_ref.rec[b_ref.n as usize] = (lat << KEY_BITS) | key;
            b_ref.n += 1;
        }
    }
}

#[inline(always)]
fn append_u32(buf: &mut [u8], mut val: u32, mut pos: usize) -> usize {
    if val == 0 { buf[pos] = b'0'; return pos + 1; }
    let mut temp = [0u8; 10]; let mut len = 0;
    while val > 0 { temp[len] = b'0' + (val % 10) as u8; val /= 10; len += 1; }
    while len > 0 { len -= 1; buf[pos] = temp[len]; pos += 1; }
    pos
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    unsafe { prctl(14, 1, 0, 0, 0); }
    let (mut data_dir, mut out_dir) = (PathBuf::from("/data"), PathBuf::from("/out"));
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--data" && i + 1 < args.len() { data_dir = PathBuf::from(&args[i + 1]); i += 2; }
        else if args[i] == "--out" && i + 1 < args.len() { out_dir = PathBuf::from(&args[i + 1]); i += 2; }
        else { i += 1; }
    }
    if let Ok(e) = std::env::var("DATA_DIR") { if !e.is_empty() { data_dir = PathBuf::from(e); } }
    if let Ok(e) = std::env::var("OUT_DIR") { if !e.is_empty() { out_dir = PathBuf::from(e); } }
    std::fs::create_dir_all(&out_dir).ok();

    let in_path = if data_dir.is_file() { data_dir.clone() } else { data_dir.join("requests.log") };
    let out_path = out_dir.join("summary.txt");
    if !in_path.exists() { File::create(&out_path)?; return Ok(()); }

    let file = File::open(&in_path)?;
    let size = file.metadata()?.len() as usize;
    if size == 0 { File::create(&out_path)?; return Ok(()); }

    let fd = file.as_raw_fd();
    let data = unsafe {
        let p = mmap(std::ptr::null_mut(), size + 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if p == MAP_FAILED || mmap(p, size, PROT_READ, MAP_PRIVATE | MAP_FIXED, fd, 0) == MAP_FAILED { _exit(1); }
        madvise(p, size, MADV_SEQUENTIAL);
        p as *const u8
    };

    let mut labels = [[0u8; 20]; HOURS];
    for h in 0..HOURS {
        let day = (h + 18) / 24;
        let (m, d) = if day < 2 { (10, 30 + day) } else { (11, day - 1) };
        let s = format!("2026-{m:02}-{d:02}T{:02}:00:00Z", (h + 18) % 24);
        labels[h].copy_from_slice(s.as_bytes());
    }

    let mut cut = vec![size; NTHREADS + 1]; cut[0] = 0;
    for t in 1..NTHREADS {
        let mut c = cut[t - 1].max(size * t / NTHREADS);
        while c < size && unsafe { *data.add(c - 1) } != b'\n' { c += 1; }
        cut[t] = c;
    }

    let total_pool = NTHREADS * BLOCKS_PER_WORKER * std::mem::size_of::<Block>();
    let pool = unsafe {
        let p = mmap(std::ptr::null_mut(), total_pool, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if p == MAP_FAILED { _exit(1); }
        p as *mut Block
    };

    let mut workers: Vec<Worker> = (0..NTHREADS).map(|t| {
        let mut w = Worker::new();
        w.pool_base = unsafe { pool.add(t * BLOCKS_PER_WORKER) };
        w.pool_cap = BLOCKS_PER_WORKER;
        w
    }).collect();

    let done: Vec<AtomicBool> = (0..HOURS).map(|_| AtomicBool::new(false)).collect();
    let mut text_boxes: Vec<Vec<u8>> = (0..HOURS).map(|_| Vec::new()).collect();
    let text_ptrs: Vec<*mut Vec<u8>> = text_boxes.iter_mut().map(|v| v as *mut Vec<u8>).collect();
    let text_send = SendPtr(text_ptrs.as_ptr());
    let mut rank = vec![Vec::new(); NTHREADS];
    let data_send = SendPtr(data);

    // Phase 1: parallel parse
    thread::scope(|s| {
        let (cut_ref, d_send) = (&cut, data_send);
        for (t, w) in workers.iter_mut().enumerate() {
            s.spawn(move || unsafe { w.parse(d_send.as_ptr().add(cut_ref[t]), d_send.as_ptr().add(cut_ref[t + 1])); });
        }
    });

    let mut global = EndpointTable::new();
    for t in 0..NTHREADS {
        for n in &workers[t].endpoints.names { rank[t].push(unsafe { global.intern(n.as_ptr(), n.len()) }); }
    }
    let mut sorted_names = global.names.clone();
    sorted_names.sort_unstable();

    let mut rank_of = vec![0u32; sorted_names.len()];
    for (i, n) in sorted_names.iter().enumerate() {
        rank_of[unsafe { global.intern(n.as_ptr(), n.len()) } as usize] = i as u32;
    }
    for r in &mut rank {
        for id in r { *id = rank_of[*id as usize]; }
    }
    let num_endpoints = sorted_names.len();

    unsafe { madvise(data as *mut u8, size + 4096, MADV_DONTNEED); }

    let next_hour = AtomicUsize::new(0);

    // Phase 2: parallel reduction + disk write
    thread::scope(|s| {
        let done_ref = &done;
        let out_p = out_path.clone();
        let t_send = text_send;
        s.spawn(move || {
            let mut out_file = match File::create(&out_p) { Ok(f) => f, Err(_) => return };
            for h in 0..HOURS {
                while !done_ref[h].load(Ordering::Acquire) { std::hint::spin_loop(); }
                let buf = unsafe { &mut *(*t_send.as_ptr().add(h)) };
                out_file.write_all(buf).ok();
                *buf = Vec::new();
            }
        });

        let (next_h_ref, workers_ref, rank_ref, labels_ref, names_ref, ne) =
            (&next_hour, &workers, &rank, &labels, &sorted_names, num_endpoints);

        for _ in 0..NTHREADS {
            s.spawn(move || {
                let (mut count, mut errors, mut cursor) = (vec![0u32; ne + 1], vec![0u32; ne], vec![0u32; ne]);
                let mut lat: Vec<u32> = Vec::new();
                let mut line = [0u8; 512];

                loop {
                    let h = next_h_ref.fetch_add(1, Ordering::Relaxed);
                    if h >= HOURS { break; }
                    count.fill(0); errors.fill(0);

                    for wt in 0..NTHREADS {
                        let (w, r) = (&workers_ref[wt], &rank_ref[wt]);
                        let mut bp = w.head[h];
                        while !bp.is_null() {
                            let b = unsafe { &*bp };
                            for i in 0..b.n as usize {
                                let key = b.rec[i] & ((1 << KEY_BITS) - 1);
                                let e = r[(key & ID_MASK) as usize] as usize;
                                count[e + 1] += 1;
                                if (key & ERR_BIT) != 0 { errors[e] += 1; }
                            }
                            bp = b.next;
                        }
                        for &(_, key) in &w.big[h] {
                            let e = r[(key & ID_MASK) as usize] as usize;
                            count[e + 1] += 1;
                            if (key & ERR_BIT) != 0 { errors[e] += 1; }
                        }
                    }

                    for e in 0..ne { count[e + 1] += count[e]; }
                    cursor.copy_from_slice(&count[..ne]);
                    let total_lat = count[ne] as usize;
                    lat.clear(); lat.reserve(total_lat);
                    unsafe { lat.set_len(total_lat); }

                    for wt in 0..NTHREADS {
                        let (w, r) = (&workers_ref[wt], &rank_ref[wt]);
                        let mut bp = w.head[h];
                        while !bp.is_null() {
                            let b = unsafe { &*bp };
                            for i in 0..b.n as usize {
                                let rec = b.rec[i];
                                let key = rec & ((1 << KEY_BITS) - 1);
                                let e = r[(key & ID_MASK) as usize] as usize;
                                lat[cursor[e] as usize] = rec >> KEY_BITS;
                                cursor[e] += 1;
                            }
                            bp = b.next;
                        }
                        for &(lat_val, key) in &w.big[h] {
                            let e = r[(key & ID_MASK) as usize] as usize;
                            lat[cursor[e] as usize] = lat_val;
                            cursor[e] += 1;
                        }
                    }

                    let mut out_text = Vec::with_capacity(ne * 80);
                    let label = &labels_ref[h];

                    for e in 0..ne {
                        let (start, end) = (count[e] as usize, count[e + 1] as usize);
                        let n = end - start;
                        if n == 0 { continue; }

                        let slice = &mut lat[start..end];
                        let (p50, p95, p99) = if n == 1 {
                            (slice[0], slice[0], slice[0])
                        } else if n == 2 {
                            let (a, b) = if slice[0] <= slice[1] { (slice[0], slice[1]) } else { (slice[1], slice[0]) };
                            (a, b, b)
                        } else if n == 3 {
                            let mut a = slice[0]; let mut b = slice[1]; let mut c = slice[2];
                            if a > b { std::mem::swap(&mut a, &mut b); }
                            if b > c { std::mem::swap(&mut b, &mut c); }
                            if a > b { std::mem::swap(&mut a, &mut b); }
                            (b, c, c)
                        } else if n <= 256 {
                            slice.sort_unstable();
                            (slice[(50 * n + 99) / 100 - 1], slice[(95 * n + 99) / 100 - 1], slice[(99 * n + 99) / 100 - 1])
                        } else {
                            let (r99, r95, r50) = ((99 * n + 99) / 100 - 1, (95 * n + 99) / 100 - 1, (50 * n + 99) / 100 - 1);
                            slice.select_nth_unstable(r99);
                            let p99 = slice[r99];
                            let p95 = if r95 == r99 { p99 } else { slice[..r99].select_nth_unstable(r95); slice[r95] };
                            let p50 = if r50 == r95 { p95 } else { slice[..r95].select_nth_unstable(r50); slice[r50] };
                            (p50, p95, p99)
                        };

                        let err = errors[e];
                        let ppm = ((err as u64) * 1_000_000 / (n as u64)) as u32;

                        let mut pos = 0;
                        line[..20].copy_from_slice(label); pos += 20;
                        line[pos] = b' '; pos += 1;
                        let ep_name = names_ref[e];
                        line[pos..pos + ep_name.len()].copy_from_slice(ep_name); pos += ep_name.len();
                        line[pos] = b' '; pos += 1; pos = append_u32(&mut line, n as u32, pos);
                        line[pos] = b' '; pos += 1; pos = append_u32(&mut line, p50, pos);
                        line[pos] = b' '; pos += 1; pos = append_u32(&mut line, p95, pos);
                        line[pos] = b' '; pos += 1; pos = append_u32(&mut line, p99, pos);
                        line[pos] = b' '; pos += 1; pos = append_u32(&mut line, err, pos);
                        line[pos] = b' '; pos += 1; pos = append_u32(&mut line, ppm, pos);
                        line[pos] = b'\n'; pos += 1;
                        out_text.extend_from_slice(&line[..pos]);
                    }

                    unsafe { (*(*t_send.as_ptr().add(h))) = out_text; }
                    done_ref[h].store(true, Ordering::Release);
                }
            });
        }
    });

    unsafe { _exit(0); }
}
