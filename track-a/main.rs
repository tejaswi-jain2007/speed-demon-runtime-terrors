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
}

#[inline(always)]
unsafe fn libc_exit(code: i32) -> ! {
    extern "C" { fn _exit(status: i32) -> !; }
    _exit(code);
}

const PROT_READ: i32 = 1; const PROT_WRITE: i32 = 2; const MAP_PRIVATE: i32 = 0x02; const MAP_ANONYMOUS: i32 = 0x20;
const MAP_FIXED: i32 = 0x10; const MAP_FAILED: *mut u8 = !0 as *mut u8; const MADV_DONTNEED: i32 = 4; const MADV_SEQUENTIAL: i32 = 2;
const HOURS: usize = 72; const KEY_BITS: u32 = 12; const ID_MASK: u32 = (1 << 11) - 1; const ERR_BIT: u32 = 1 << 11;
const BIG: u32 = 1 << 20; const NTHREADS: usize = 12; const BLOCK_CAP: usize = (16384 - 16) / 4; const TABLE_SLOTS: usize = 8192;

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
unsafe fn parse_lat(p: *const u8, len: usize) -> u32 {
    let mut v = 0u32;
    for i in 0..len { v = v * 10 + (*p.add(i) - b'0') as u32; }
    v
}

#[derive(Copy, Clone)] struct SendPtr<T>(*const T);
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}
impl<T> SendPtr<T> { #[inline(always)] fn as_ptr(&self) -> *const T { self.0 } }

struct EndpointTable<'a> { slots: [u32; TABLE_SLOTS], names: Vec<&'a [u8]> }

impl<'a> EndpointTable<'a> {
    fn new() -> Self { Self { slots: [0; TABLE_SLOTS], names: Vec::with_capacity(2048) } }

    #[inline(always)]
    unsafe fn intern(&mut self, s: *const u8, len: usize) -> u32 {
        let mut a = load8(s);
        let b = if len >= 8 { load8(s.add(len - 8)) } else { 0 };
        if len < 8 { a &= (1u64 << (8 * len)) - 1; }
        let h1 = _mm_crc32_u64(len as u64, a);
        let mut i = _mm_crc32_u64(h1, b) as usize;
        loop {
            let slot = &mut self.slots[i & (TABLE_SLOTS - 1)];
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
struct Block { next: *mut Block, n: u32, rec: [u32; BLOCK_CAP] }

struct Worker<'a> {
    endpoints: EndpointTable<'a>,
    head: [*mut Block; HOURS],
    big: Vec<Vec<(u32, u32)>>,
    region: *mut u8,
    used: usize,
    reserved: usize,
}
unsafe impl<'a> Send for Worker<'a> {}
unsafe impl<'a> Sync for Worker<'a> {}

impl<'a> Worker<'a> {
    fn new() -> Self {
        Self {
            endpoints: EndpointTable::new(),
            head: [std::ptr::null_mut(); HOURS],
            big: (0..HOURS).map(|_| Vec::new()).collect(),
            region: std::ptr::null_mut(),
            used: 0,
            reserved: 0,
        }
    }

    unsafe fn parse(&mut self, mut p: *const u8, end: *const u8) {
        if p >= end { return; }
        let start_p = p;
        let reserve = 16 * std::mem::size_of::<Block>();
        self.reserved = reserve;
        self.region = mmap(std::ptr::null_mut(), reserve, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if self.region == MAP_FAILED { libc_exit(1); }

        let space_vec = _mm256_set1_epi8(b' ' as i8);
        let nl_vec = _mm256_set1_epi8(b'\n' as i8);
        let mut curr_ts4 = 0u32;
        let mut curr_h = 0usize;
        let mut curr_block = std::ptr::null_mut::<Block>();

        while p < end {
            let ts4 = (p.add(9) as *const u32).read_unaligned();
            let h = if ts4 == curr_ts4 {
                curr_h
            } else {
                let day_idx = if *p.add(6) == b'0' { (*p.add(9) - b'0') as i32 } else { (*p.add(9) - b'0' + 1) as i32 };
                let hour = ((*p.add(11) - b'0') * 10 + (*p.add(12) - b'0')) as i32;
                curr_h = (day_idx * 24 + hour - 18) as usize;
                curr_ts4 = ts4;
                curr_block = self.head[curr_h];
                curr_h
            };

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
            let sp2 = sp & (sp - 1);
            let ee = if sp2 != 0 { m.add(sp2.trailing_zeros() as usize) } else { find(ep, b' ') };
            let eol = if nl != 0 { m.add(nl.trailing_zeros() as usize) } else { find(ee.add(5), b'\n') };

            let is_err = if *ee.add(1) == b'5' { ERR_BIT } else { 0 };
            let key = self.endpoints.intern(ep, ee as usize - ep as usize) | is_err;
            let lat = parse_lat(ee.add(5), eol as usize - ee as usize - 5);
            p = eol.add(1);

            if lat >= BIG { self.big[h].push((lat, key)); continue; }

            if curr_block.is_null() || (*curr_block).n as usize == BLOCK_CAP {
                if self.used + std::mem::size_of::<Block>() > self.reserved {
                    let extra = 16 * std::mem::size_of::<Block>();
                    let p_m = mmap(std::ptr::null_mut(), extra, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
                    if p_m != MAP_FAILED { self.region = p_m; self.used = 0; self.reserved = extra; }
                }
                let fresh = self.region.add(self.used) as *mut Block;
                self.used += std::mem::size_of::<Block>();
                (*fresh).next = self.head[h];
                (*fresh).n = 0;
                self.head[h] = fresh;
                curr_block = fresh;
            }
            let b_ref = &mut *curr_block;
            b_ref.rec[b_ref.n as usize] = (lat << KEY_BITS) | key;
            b_ref.n += 1;
        }

        let sf = (start_p as usize + 4095) & !4095;
        let ef = (end as usize) & !4095;
        if ef > sf { madvise(sf as *mut u8, ef - sf, MADV_DONTNEED); }
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
        if p == MAP_FAILED || mmap(p, size, PROT_READ, MAP_PRIVATE | MAP_FIXED, fd, 0) == MAP_FAILED { libc_exit(1); }
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

    let mut workers: Vec<Worker> = (0..NTHREADS).map(|_| Worker::new()).collect();
    let done: Vec<AtomicBool> = (0..HOURS).map(|_| AtomicBool::new(false)).collect();
    let text_boxes: Vec<std::sync::Mutex<Vec<u8>>> = (0..HOURS).map(|_| std::sync::Mutex::new(Vec::new())).collect();
    let mut rank = vec![Vec::new(); NTHREADS];
    let data_send = SendPtr(data);

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
    let mut sorted_names: Vec<Vec<u8>> = global.names.iter().map(|s| s.to_vec()).collect();
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

    thread::scope(|s| {
        let (done_ref, text_ref, out_p) = (&done, &text_boxes, out_path.clone());
        s.spawn(move || {
            let mut out_file = match File::create(&out_p) { Ok(f) => f, Err(_) => return };
            for h in 0..HOURS {
                while !done_ref[h].load(Ordering::Acquire) { std::hint::spin_loop(); }
                let mut buf = text_ref[h].lock().unwrap();
                out_file.write_all(&buf).ok();
                *buf = Vec::new();
            }
        });

        let (next_h_ref, done_ref, text_ref, workers_ref, rank_ref, labels_ref, names_ref, ne) =
            (&next_hour, &done, &text_boxes, &workers, &rank, &labels, &sorted_names, num_endpoints);

        for _ in 0..NTHREADS {
            s.spawn(|| {
                let (mut count, mut errors, mut cursor) = (vec![0u32; ne + 1], vec![0u32; ne], vec![0u32; ne]);
                let mut lat: Vec<u32> = Vec::new();
                let mut line = [0u8; 512];

                loop {
                    let h = next_h_ref.fetch_add(1, Ordering::Relaxed);
                    if h >= HOURS { break; }

                    count.fill(0); errors.fill(0);
                    for wt in 0..NTHREADS {
                        let (w, r) = (&workers_ref[wt], &rank_ref[wt]);
                        let mut b = w.head[h];
                        while !b.is_null() {
                            unsafe {
                                let b_ref = &*b;
                                for i in 0..b_ref.n as usize {
                                    let rec = b_ref.rec[i];
                                    let e = r[(rec & ID_MASK) as usize] as usize;
                                    count[e + 1] += 1;
                                    if (rec & ERR_BIT) != 0 { errors[e] += 1; }
                                }
                                b = b_ref.next;
                            }
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
                        let mut b = w.head[h];
                        while !b.is_null() {
                            unsafe {
                                let b_ref = &*b;
                                for i in 0..b_ref.n as usize {
                                    let rec = b_ref.rec[i];
                                    let e = r[(rec & ID_MASK) as usize] as usize;
                                    lat[cursor[e] as usize] = rec >> KEY_BITS;
                                    cursor[e] += 1;
                                }
                                let next_b = b_ref.next;
                                madvise(b as *mut u8, std::mem::size_of::<Block>(), MADV_DONTNEED);
                                b = next_b;
                            }
                        }
                        for &(l, key) in &w.big[h] {
                            let e = r[(key & ID_MASK) as usize] as usize;
                            lat[cursor[e] as usize] = l;
                            cursor[e] += 1;
                        }
                    }

                    let label = &labels_ref[h];
                    let mut out_text = text_ref[h].lock().unwrap();
                    out_text.reserve(ne * 64);

                    for e in 0..ne {
                        let (start, end) = (count[e] as usize, count[e + 1] as usize);
                        let n = end - start;
                        if n == 0 { continue; }

                        let slice = &mut lat[start..end];
                        let (p50, p95, p99) = if n == 1 {
                            let a = slice[0]; (a, a, a)
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
                        let ep_name = &names_ref[e];
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

                    done_ref[h].store(true, Ordering::Release);
                }
            });
        }
    });

    unsafe { libc_exit(0); }
}
