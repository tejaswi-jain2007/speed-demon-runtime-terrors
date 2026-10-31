// Speed Demon 2026 - Track A (Ingest)
// Team: Runtime Terrors
use std::arch::x86_64::*;
use std::cell::UnsafeCell;
use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Barrier;
use std::thread;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    fn prctl(opt: i32, a2: u64, a3: u64, a4: u64, a5: u64) -> i32;
}

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_PRIVATE: i32 = 0x02;
const MAP_ANONYMOUS: i32 = 0x20;
const MAP_FAILED: *mut u8 = !0 as *mut u8;
const MADV_DONTNEED: i32 = 4;

const HOURS: usize = 72;
const KEY_BITS: u32 = 12;
const ID_MASK: u32 = (1 << 11) - 1;
const ERR_BIT: u32 = 1 << 11;
const BIG: u32 = 1 << 20;
const NTHREADS: usize = 12;
const BLOCK_CAP: usize = (8192 - 16) / 4;

#[inline(always)]
unsafe fn load8(p: *const u8) -> u64 { (p as *const u64).read_unaligned() }

#[inline(always)]
unsafe fn match32(p: *const u8, c: u8) -> u32 {
    let v = _mm256_loadu_si256(p as *const __m256i);
    _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(c as i8))) as u32
}

#[inline(always)]
unsafe fn find(mut s: *const u8, c: u8) -> *const u8 {
    loop {
        let m = match32(s, c);
        if m != 0 { return s.add(m.trailing_zeros() as usize); }
        s = s.add(32);
    }
}

#[inline(always)]
unsafe fn parse_digits(end: *const u8, len: u32) -> u32 {
    let v = _mm_subs_epu8(_mm_loadu_si128(end.sub(16) as *const __m128i), _mm_set1_epi8(b'0' as i8));
    let lane = _mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    let v = _mm_and_si128(v, _mm_cmpgt_epi8(lane, _mm_set1_epi8((15 - len) as i8)));
    let v = _mm_maddubs_epi16(v, _mm_set1_epi16(0x010A));
    let v = _mm_madd_epi16(v, _mm_set1_epi32(0x00010064));
    let v = _mm_packus_epi32(v, v);
    let v = _mm_madd_epi16(v, _mm_set1_epi32(0x00012710));
    let high = _mm_cvtsi128_si32(v) as u32;
    let low = _mm_extract_epi32(v, 1) as u32;
    high.wrapping_mul(100000000) + low
}

#[derive(Copy, Clone)]
struct SendPtr<T>(*const T);
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}

const TABLE_SLOTS: usize = 256;

struct EndpointTable<'a> {
    slots: [u32; TABLE_SLOTS],
    names: Vec<&'a [u8]>,
}

impl<'a> EndpointTable<'a> {
    fn new() -> Self { Self { slots: [0; TABLE_SLOTS], names: Vec::with_capacity(32) } }

    #[inline(always)]
    unsafe fn intern(&mut self, s: *const u8, len: usize) -> u32 {
        let mut a = load8(s);
        let b = if len >= 8 { load8(s.add(len - 8)) } else { 0 };
        if len < 8 { a &= (1u64 << (8 * len)) - 1; }
        let h1 = _mm_crc32_u64(len as u64, a);
        let mut i = _mm_crc32_u64(h1, b) as usize;
        loop {
            let slot = &mut self.slots[i % TABLE_SLOTS];
            if *slot == 0 {
                let idx = self.names.len() as u32;
                self.names.push(std::slice::from_raw_parts(s, len));
                *slot = idx + 1;
                return idx;
            }
            let n = self.names[(*slot - 1) as usize];
            if n.len() == len && std::slice::from_raw_parts(s, len) == n { return *slot - 1; }
            i += 1;
        }
    }
}

#[repr(C)]
struct Block {
    next: *mut Block,
    n: u32,
    rec: [u32; BLOCK_CAP],
}

struct Worker<'a> {
    endpoints: EndpointTable<'a>,
    head: [*mut Block; HOURS],
    big: Vec<Vec<(u32, u32)>>,
    pool: *mut u8,
    pool_used: usize,
    pool_cap: usize,
}

struct WorkerCell<'a>(UnsafeCell<Worker<'a>>);
unsafe impl<'a> Sync for WorkerCell<'a> {}

impl<'a> Worker<'a> {
    fn new() -> Self {
        Self {
            endpoints: EndpointTable::new(),
            head: [std::ptr::null_mut(); HOURS],
            big: (0..HOURS).map(|_| Vec::new()).collect(),
            pool: std::ptr::null_mut(),
            pool_used: 0,
            pool_cap: 0,
        }
    }

    unsafe fn alloc_block(&mut self) -> *mut Block {
        let sz = std::mem::size_of::<Block>();
        if self.pool_used + sz > self.pool_cap {
            let chunk = 256 * 1024;
            let p = mmap(std::ptr::null_mut(), chunk, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            if p == MAP_FAILED { std::process::exit(1); }
            self.pool = p;
            self.pool_used = 0;
            self.pool_cap = chunk;
        }
        let b = self.pool.add(self.pool_used) as *mut Block;
        self.pool_used += sz;
        (*b).next = std::ptr::null_mut();
        (*b).n = 0;
        b
    }

    unsafe fn parse(&mut self, mut p: *const u8, end: *const u8) {
        while p < end {
            let day_idx = if *p.add(6) == b'0' { (*p.add(9) - b'0') as i32 } else { (*p.add(9) - b'0' + 1) as i32 };
            let hour = ((*p.add(11) - b'0') * 10 + (*p.add(12) - b'0')) as i32;
            let h = (day_idx * 24 + hour - 18) as usize;

            let m = p.add(28);
            let sp = (match32(m, b' ') as u64) | ((match32(m.add(32), b' ') as u64) << 32);
            let nl = (match32(m, b'\n') as u64) | ((match32(m.add(32), b'\n') as u64) << 32);
            let ep = m.add(sp.trailing_zeros() as usize + 1);
            let ee = if nl != 0 { m.add((sp & (sp - 1)).trailing_zeros() as usize) } else { find(ep, b' ') };
            let eol = if nl != 0 { m.add(nl.trailing_zeros() as usize) } else { find(ee.add(5), b'\n') };

            let is_err = if *ee.add(1) == b'5' { ERR_BIT } else { 0 };
            let key = self.endpoints.intern(ep, ee as usize - ep as usize) | is_err;
            let lat = parse_digits(eol, (eol as usize - ee as usize - 5) as u32);
            p = eol.add(1);

            if lat >= BIG {
                self.big[h].push((lat, key));
                continue;
            }

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
    let mut temp = [0u8; 10];
    let mut len = 0;
    while val > 0 { temp[len] = b'0' + (val % 10) as u8; val /= 10; len += 1; }
    while len > 0 { len -= 1; buf[pos] = temp[len]; pos += 1; }
    pos
}

struct SharedState<'a> {
    names: Vec<&'a [u8]>,
    rank: [[u32; 256]; NTHREADS],
    num_endpoints: usize,
}
struct SyncShared<'a>(UnsafeCell<SharedState<'a>>);
unsafe impl<'a> Sync for SyncShared<'a> {}

struct SyncOutBufs(UnsafeCell<Vec<Vec<u8>>>);
unsafe impl Sync for SyncOutBufs {}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    unsafe { prctl(14, 1, 0, 0, 0); }

    let mut data_dir = PathBuf::from("/data");
    let mut out_dir = PathBuf::from("/out");
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
    let in_path = if data_dir.is_file() { data_dir } else { data_dir.join("requests.log") };
    let out_path = out_dir.join("summary.txt");

    if !in_path.exists() { File::create(&out_path)?; return Ok(()); }
    let file = File::open(&in_path)?;
    let size = file.metadata()?.len() as usize;
    if size == 0 { File::create(&out_path)?; return Ok(()); }

    let fd = file.as_raw_fd();
    let data = unsafe {
        let p = mmap(std::ptr::null_mut(), size, PROT_READ, MAP_PRIVATE, fd, 0);
        if p == MAP_FAILED { std::process::exit(1); }
        p as *const u8
    };

    let mut labels = [[0u8; 20]; HOURS];
    for h in 0..HOURS {
        let day = (h + 18) / 24;
        let m = if day < 2 { 10 } else { 11 };
        let d = if day < 2 { 30 + day } else { day - 1 };
        let hr = (h + 18) % 24;
        let s = format!("2026-{m:02}-{d:02}T{hr:02}:00:00Z");
        labels[h].copy_from_slice(s.as_bytes());
    }

    let mut cut = [0usize; NTHREADS + 1];
    for t in 1..NTHREADS {
        let mut c = cut[t - 1].max(size * t / NTHREADS);
        while c < size && unsafe { *data.add(c - 1) } != b'\n' { c += 1; }
        cut[t] = c;
    }
    cut[NTHREADS] = size;

    let workers: Vec<WorkerCell> = (0..NTHREADS).map(|_| WorkerCell(UnsafeCell::new(Worker::new()))).collect();
    let barrier = Barrier::new(NTHREADS);
    let next_hour = AtomicUsize::new(0);
    let shared = SyncShared(UnsafeCell::new(SharedState { names: Vec::new(), rank: [[0; 256]; NTHREADS], num_endpoints: 0 }));
    let out_bufs = SyncOutBufs(UnsafeCell::new((0..HOURS).map(|_| Vec::with_capacity(2048)).collect()));

    let d_send = SendPtr(data);
    let cut_ref = &cut;
    let barrier_ref = &barrier;
    let workers_ref = &workers;
    let next_h_ref = &next_hour;
    let shared_ref = &shared;
    let out_bufs_ref = &out_bufs;
    let labels_ref = &labels;

    thread::scope(|s| {
        for t in 0..NTHREADS {
            s.spawn(move || unsafe {
                let from = d_send.0.add(cut_ref[t]);
                let to = d_send.0.add(cut_ref[t + 1]);
                let w = &mut *workers_ref[t].0.get();
                w.parse(from, to);

                let sf = (from as usize + 4095) & !4095;
                let ef = (to as usize) & !4095;
                if ef > sf { madvise(sf as *mut u8, ef - sf, MADV_DONTNEED); }

                barrier_ref.wait();

                if t == 0 {
                    let state = &mut *shared_ref.0.get();
                    let mut global = EndpointTable::new();
                    let mut temp_rank = vec![Vec::new(); NTHREADS];
                    for wt in 0..NTHREADS {
                        let w_other = &*workers_ref[wt].0.get();
                        for n in &w_other.endpoints.names {
                            temp_rank[wt].push(global.intern(n.as_ptr(), n.len()));
                        }
                    }
                    state.names = global.names.clone();
                    state.names.sort_unstable();
                    let mut rank_of = vec![0u32; state.names.len()];
                    for (i, n) in state.names.iter().enumerate() {
                        let gid = global.intern(n.as_ptr(), n.len()) as usize;
                        rank_of[gid] = i as u32;
                    }
                    for wt in 0..NTHREADS {
                        for (local_id, &gid) in temp_rank[wt].iter().enumerate() {
                            state.rank[wt][local_id] = rank_of[gid as usize];
                        }
                    }
                    state.num_endpoints = state.names.len();
                    madvise(d_send.0 as *mut u8, size, MADV_DONTNEED);
                }

                barrier_ref.wait();

                let state = &*shared_ref.0.get();
                let ne = state.num_endpoints;
                let mut count = [0u32; 256];
                let mut errors = [0u32; 256];
                let mut cursor = [0u32; 256];
                let mut lat = Vec::new();
                let mut line = [0u8; 512];

                loop {
                    let h = next_h_ref.fetch_add(1, Ordering::Relaxed);
                    if h >= HOURS { break; }

                    count[..=ne].fill(0);
                    errors[..ne].fill(0);

                    for wt in 0..NTHREADS {
                        let w_other = &*workers_ref[wt].0.get();
                        let r = &state.rank[wt];
                        let mut b = w_other.head[h];
                        while !b.is_null() {
                            let b_ref = &*b;
                            for i in 0..b_ref.n as usize {
                                let key = b_ref.rec[i];
                                let e = r[(key & ID_MASK) as usize] as usize;
                                count[e + 1] += 1;
                                errors[e] += ((key & ERR_BIT) != 0) as u32;
                            }
                            b = b_ref.next;
                        }
                        for &(l, key) in &w_other.big[h] {
                            let _ = l;
                            let e = r[(key & ID_MASK) as usize] as usize;
                            count[e + 1] += 1;
                            errors[e] += ((key & ERR_BIT) != 0) as u32;
                        }
                    }

                    for e in 0..ne { count[e + 1] += count[e]; }
                    cursor[..ne].copy_from_slice(&count[..ne]);
                    let total_lat = count[ne] as usize;
                    lat.clear();
                    lat.reserve(total_lat);
                    lat.set_len(total_lat);

                    for wt in 0..NTHREADS {
                        let w_other = &*workers_ref[wt].0.get();
                        let r = &state.rank[wt];
                        let mut b = w_other.head[h];
                        while !b.is_null() {
                            let b_ref = &*b;
                            for i in 0..b_ref.n as usize {
                                let rec = b_ref.rec[i];
                                let e = r[(rec & ID_MASK) as usize] as usize;
                                let dst = cursor[e] as usize;
                                lat[dst] = rec >> KEY_BITS;
                                cursor[e] += 1;
                            }
                            let next_b = b_ref.next;
                            madvise(b as *mut u8, std::mem::size_of::<Block>(), MADV_DONTNEED);
                            b = next_b;
                        }
                        for &(l, key) in &w_other.big[h] {
                            let e = r[(key & ID_MASK) as usize] as usize;
                            let dst = cursor[e] as usize;
                            lat[dst] = l;
                            cursor[e] += 1;
                        }
                    }

                    let out_buf = &mut (*out_bufs_ref.0.get())[h];
                    out_buf.reserve(ne * 64);

                    for e in 0..ne {
                        let start = count[e] as usize;
                        let end = count[e + 1] as usize;
                        let n = end - start;
                        if n == 0 { continue; }

                        let slice = &mut lat[start..end];
                        let (p50, p95, p99) = if n <= 256 {
                            slice.sort_unstable();
                            (slice[(50 * n + 99) / 100 - 1], slice[(95 * n + 99) / 100 - 1], slice[(99 * n + 99) / 100 - 1])
                        } else {
                            let r99 = (99 * n + 99) / 100 - 1;
                            let r95 = (95 * n + 99) / 100 - 1;
                            let r50 = (50 * n + 99) / 100 - 1;
                            slice.select_nth_unstable(r99);
                            let p99 = slice[r99];
                            let p95 = if r95 == r99 { p99 } else { slice[..r99].select_nth_unstable(r95); slice[r95] };
                            let p50 = if r50 == r95 { p95 } else { slice[..r95].select_nth_unstable(r50); slice[r50] };
                            (p50, p95, p99)
                        };

                        let err = errors[e];
                        let ppm = ((err as u64) * 1_000_000 / (n as u64)) as u32;

                        let mut pos = 0;
                        line[..20].copy_from_slice(&labels_ref[h]); pos += 20;
                        line[pos] = b' '; pos += 1;
                        let ep_name = state.names[e];
                        line[pos..pos + ep_name.len()].copy_from_slice(ep_name); pos += ep_name.len();
                        line[pos] = b' '; pos += 1; pos = append_u32(&mut line, n as u32, pos);
                        line[pos] = b' '; pos += 1; pos = append_u32(&mut line, p50, pos);
                        line[pos] = b' '; pos += 1; pos = append_u32(&mut line, p95, pos);
                        line[pos] = b' '; pos += 1; pos = append_u32(&mut line, p99, pos);
                        line[pos] = b' '; pos += 1; pos = append_u32(&mut line, err, pos);
                        line[pos] = b' '; pos += 1; pos = append_u32(&mut line, ppm, pos);
                        line[pos] = b'\n'; pos += 1;
                        out_buf.extend_from_slice(&line[..pos]);
                    }
                }
            });
        }
    });

    let mut out_file = File::create(&out_path)?;
    let all_bufs = unsafe { &*out_bufs.0.get() };
    for h in 0..HOURS {
        out_file.write_all(&all_bufs[h])?;
    }
    out_file.flush()?;

    unsafe { libc_exit(0); }
}

unsafe fn libc_exit(code: i32) -> ! {
    extern "C" { fn _exit(status: i32) -> !; }
    _exit(code);
}
