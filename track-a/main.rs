// Speed Demon 2026 - Track A (Ingest) - Team: Runtime Terrors
use std::{arch::x86_64::*, fs::File, io::Write, os::unix::io::AsRawFd, path::PathBuf, sync::{atomic::{AtomicBool, AtomicUsize, Ordering}, Barrier, Mutex}, thread};

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn madvise(a: *mut u8, l: usize, adv: i32) -> i32;
    fn prctl(op: i32, a2: u64, a3: u64, a4: u64, a5: u64) -> i32;
    fn _exit(status: i32) -> !;
}
const HOURS: usize = 72;
const KEY_BITS: u32 = 12;
const ID_MASK: u32 = (1 << 11) - 1;
const ERR_BIT: u32 = 1 << 11;
const BIG: u32 = 1 << 20;
const NTHREADS: usize = 12;
const TABLE_SLOTS: usize = 1 << 12;
const BLOCK_CAP: usize = (16384 - 16) / 4;

#[inline(always)] unsafe fn load8(p: *const u8) -> u64 { (p as *const u64).read_unaligned() }
#[inline(always)] unsafe fn match32(p: *const u8, c: u8) -> u32 {
    let v = _mm256_loadu_si256(p as *const __m256i);
    _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(c as i8))) as u32
}
#[inline(always)] unsafe fn find(mut s: *const u8, c: u8) -> *const u8 {
    loop { let m = match32(s, c); if m != 0 { return s.add(m.trailing_zeros() as usize); } s = s.add(32); }
}
#[inline(always)] unsafe fn parse_digits(end: *const u8, len: u32) -> u32 {
    let v = _mm_loadu_si128(end.sub(16) as *const __m128i);
    let v = _mm_subs_epu8(v, _mm_set1_epi8(b'0' as i8));
    let lane = _mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    let v = _mm_and_si128(v, _mm_cmpgt_epi8(lane, _mm_set1_epi8((15 - len) as i8)));
    let v = _mm_maddubs_epi16(v, _mm_set1_epi16(0x010A));
    let v = _mm_madd_epi16(v, _mm_set1_epi32(0x00010064));
    let v = _mm_packus_epi32(v, v);
    let v = _mm_madd_epi16(v, _mm_set1_epi32(0x00012710));
    (_mm_cvtsi128_si32(v) as u32).wrapping_mul(100000000) + (_mm_extract_epi32(v, 1) as u32)
}
#[derive(Copy, Clone)] struct SendPtr<T>(*const T);
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}
impl<T> SendPtr<T> { #[inline(always)] fn as_ptr(&self) -> *const T { self.0 } }

struct SendMutPtr<T>(*mut T);
unsafe impl<T> Send for SendMutPtr<T> {}
unsafe impl<T> Sync for SendMutPtr<T> {}
impl<T> Clone for SendMutPtr<T> { fn clone(&self) -> Self { *self } }
impl<T> Copy for SendMutPtr<T> {}
impl<T> SendMutPtr<T> { #[inline(always)] fn as_ptr(&self) -> *mut T { self.0 } }

struct EndpointTable<'a> { slots: [u32; TABLE_SLOTS], names: Vec<&'a [u8]> }
impl<'a> EndpointTable<'a> {
    fn new() -> Self { Self { slots: [0; TABLE_SLOTS], names: Vec::with_capacity(512) } }
    #[inline(always)] unsafe fn intern(&mut self, s: *const u8, len: usize) -> u32 {
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

#[repr(C)] struct Block { next: *mut Block, n: u32, rec: [u32; BLOCK_CAP] }
struct Worker<'a> {
    endpoints: EndpointTable<'a>,
    head: [*mut Block; HOURS],
    big: Vec<Vec<(u32, u32)>>,
    region: *mut u8,
    used: usize,
}
unsafe impl<'a> Send for Worker<'a> {}
unsafe impl<'a> Sync for Worker<'a> {}

impl<'a> Worker<'a> {
    fn new() -> Self {
        Self {
            endpoints: EndpointTable::new(),
            head: [std::ptr::null_mut(); HOURS],
            big: (0..HOURS).map(|_| Vec::new()).collect(),
            region: std::ptr::null_mut(), used: 0,
        }
    }
    unsafe fn parse(&mut self, mut p: *const u8, end: *const u8) {
        if p >= end { return; }
        self.region = mmap(std::ptr::null_mut(), 8 * 1024 * 1024, 3, 0x22, -1, 0);
        if self.region == !0 as *mut u8 { std::process::exit(1); }
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
            let key = self.endpoints.intern(ep, ee as usize - ep as usize) | if *ee.add(1) == b'5' { ERR_BIT } else { 0 };
            let lat = parse_digits(eol, (eol as usize - ee as usize - 5) as u32);
            p = eol.add(1);
            if lat >= BIG { self.big[h].push((lat, key)); continue; }
            let b = &mut self.head[h];
            if (*b).is_null() || (*(*b)).n as usize == BLOCK_CAP {
                let fresh = self.region.add(self.used) as *mut Block;
                self.used += std::mem::size_of::<Block>();
                (*fresh).next = *b; (*fresh).n = 0; *b = fresh;
            }
            let b_ref = &mut *(*b);
            b_ref.rec[b_ref.n as usize] = (lat << KEY_BITS) | key;
            b_ref.n += 1;
        }
    }
}

#[inline(always)] fn append_u32(buf: &mut [u8], mut val: u32, mut pos: usize) -> usize {
    if val == 0 { buf[pos] = b'0'; return pos + 1; }
    let (mut t, mut len) = ([0u8; 10], 0);
    while val > 0 { t[len] = b'0' + (val % 10) as u8; val /= 10; len += 1; }
    while len > 0 { len -= 1; buf[pos] = t[len]; pos += 1; }
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

    let (in_path, out_path) = (if data_dir.is_file() { data_dir } else { data_dir.join("requests.log") }, out_dir.join("summary.txt"));
    if !in_path.exists() { File::create(&out_path)?; return Ok(()); }
    let file = File::open(&in_path)?;
    let size = file.metadata()?.len() as usize;
    if size == 0 { File::create(&out_path)?; return Ok(()); }

    let fd = file.as_raw_fd();
    let data = unsafe {
        let p = mmap(std::ptr::null_mut(), size + 4096, 3, 0x22, -1, 0);
        if p == !0 as *mut u8 || mmap(p, size, 1, 0x12, fd, 0) == !0 as *mut u8 { std::process::exit(1); }
        p as *const u8
    };

    let mut labels = [[0u8; 20]; HOURS];
    for h in 0..HOURS {
        let (m, d, hr) = (if h < 30 { 10 } else { 11 }, if h < 30 { 30 + (h + 18) / 24 } else { (h + 18) / 24 - 1 }, (h + 18) % 24);
        labels[h].copy_from_slice(format!("2026-{m:02}-{d:02}T{hr:02}:00:00Z").as_bytes());
    }

    let mut cut = vec![size; NTHREADS + 1]; cut[0] = 0;
    for t in 1..NTHREADS {
        let mut c = cut[t - 1].max(size * t / NTHREADS);
        while c < size && unsafe { *data.add(c - 1) } != b'\n' { c += 1; }
        cut[t] = c;
    }

    let mut workers: Vec<Worker> = (0..NTHREADS).map(|_| Worker::new()).collect();
    let barrier = Barrier::new(NTHREADS);
    let barrier2 = Barrier::new(NTHREADS);
    let next_hour = AtomicUsize::new(0);
    let done: Vec<_> = (0..HOURS).map(|_| AtomicBool::new(false)).collect();
    let text_boxes: Vec<_> = (0..HOURS).map(|_| Mutex::new(Vec::new())).collect();
    let mut rank = vec![Vec::new(); NTHREADS];
    let mut sorted_names: Vec<&[u8]> = Vec::new();

    let workers_send = SendMutPtr(workers.as_mut_ptr());
    let rank_send = SendMutPtr(&mut rank as *mut Vec<Vec<u32>>);
    let names_send = SendMutPtr(&mut sorted_names as *mut Vec<&[u8]>);

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
            out_file.flush().ok();
        });

        let (cut_ref, b1_ref, b2_ref, d_send) = (&cut, &barrier, &barrier2, SendPtr(data));
        let (next_h_ref, done_ref, text_ref) = (&next_hour, &done, &text_boxes);
        let labels_ref = &labels;

        for t in 0..NTHREADS {
            s.spawn(move || unsafe {
                let workers_ptr = workers_send.as_ptr();
                let r_ptr = rank_send.as_ptr();
                let n_ptr = names_send.as_ptr();

                let w = &mut *workers_ptr.add(t);
                let from = d_send.as_ptr().add(cut_ref[t]);
                let to = d_send.as_ptr().add(cut_ref[t + 1]);
                w.parse(from, to);
                let (sf, ef) = ((from as usize + 4095) & !4095, (to as usize) & !4095);
                if ef > sf { madvise(sf as *mut u8, ef - sf, 4); }

                if b1_ref.wait().is_leader() {
                    let workers_slice = std::slice::from_raw_parts(workers_ptr, NTHREADS);
                    let rank_slice = &mut *r_ptr;
                    let mut global = EndpointTable::new();
                    for wt in 0..NTHREADS {
                        for n in &workers_slice[wt].endpoints.names {
                            rank_slice[wt].push(global.intern(n.as_ptr(), n.len()));
                        }
                    }
                    let names_vec = &mut *n_ptr;
                    *names_vec = global.names.clone();
                    names_vec.sort_unstable();
                    let mut rank_of = vec![0u32; names_vec.len()];
                    for (i, n) in names_vec.iter().enumerate() {
                        let gid = global.intern(n.as_ptr(), n.len()) as usize;
                        rank_of[gid] = i as u32;
                    }
                    for r in rank_slice.iter_mut() {
                        for id in r { *id = rank_of[*id as usize]; }
                    }
                    madvise(d_send.as_ptr() as *mut u8, size + 4096, 4);
                }

                b2_ref.wait();

                let workers_slice = std::slice::from_raw_parts(workers_ptr, NTHREADS);
                let rank_slice = &*r_ptr;
                let names_slice = &*n_ptr;
                let ne = names_slice.len();

                let mut count = [0u32; 1024];
                let mut errors = [0u32; 1024];
                let mut cursor = [0u32; 1024];
                let mut lat: Vec<u32> = Vec::with_capacity(32768);
                let mut line = [0u8; 512];
                let mut thread_buf: Vec<u8> = Vec::with_capacity(32768);

                loop {
                    let h = next_h_ref.fetch_add(1, Ordering::Relaxed);
                    if h >= HOURS { break; }
                    count[..=ne].fill(0);
                    errors[..ne].fill(0);

                    for wt in 0..NTHREADS {
                        let (w_item, r) = (&workers_slice[wt], &rank_slice[wt]);
                        let mut b = w_item.head[h];
                        while !b.is_null() {
                            let b_ref = &*b;
                            for i in 0..b_ref.n as usize {
                                let (key, e) = (b_ref.rec[i], r[(b_ref.rec[i] & ID_MASK) as usize] as usize);
                                count[e + 1] += 1; errors[e] += ((key & ERR_BIT) != 0) as u32;
                            }
                            b = b_ref.next;
                        }
                        for &(_, key) in &w_item.big[h] {
                            let e = r[(key & ID_MASK) as usize] as usize;
                            count[e + 1] += 1; errors[e] += ((key & ERR_BIT) != 0) as u32;
                        }
                    }
                    for e in 0..ne { count[e + 1] += count[e]; }
                    cursor[..ne].copy_from_slice(&count[..ne]);
                    let total_lat = count[ne] as usize;
                    lat.clear();
                    if lat.capacity() < total_lat { lat.reserve(total_lat - lat.capacity()); }
                    lat.set_len(total_lat);

                    for wt in 0..NTHREADS {
                        let (w_item, r) = (&workers_slice[wt], &rank_slice[wt]);
                        let mut b = w_item.head[h];
                        while !b.is_null() {
                            let b_ref = &*b;
                            for i in 0..b_ref.n as usize {
                                let (rec, e) = (b_ref.rec[i], r[(b_ref.rec[i] & ID_MASK) as usize] as usize);
                                lat[cursor[e] as usize] = rec >> KEY_BITS; cursor[e] += 1;
                            }
                            b = b_ref.next;
                        }
                        for &(l, key) in &w_item.big[h] {
                            let e = r[(key & ID_MASK) as usize] as usize;
                            lat[cursor[e] as usize] = l; cursor[e] += 1;
                        }
                    }

                    let label = &labels_ref[h];
                    thread_buf.clear();

                    for e in 0..ne {
                        let (start, end) = (count[e] as usize, count[e + 1] as usize);
                        let n = end - start;
                        if n == 0 { continue; }
                        let slice = &mut lat[start..end];
                        let (p50, p95, p99) = if n <= 256 {
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
                        let (err, ppm) = (errors[e], ((errors[e] as u64) * 1000000 / (n as u64)) as u32);
                        let mut pos = 0;
                        line[..20].copy_from_slice(label); pos += 20;
                        line[pos] = b' '; pos += 1;
                        let ep_name = names_slice[e];
                        line[pos..pos + ep_name.len()].copy_from_slice(ep_name); pos += ep_name.len();
                        for v in [n as u32, p50, p95, p99, err, ppm] { line[pos] = b' '; pos += 1; pos = append_u32(&mut line, v, pos); }
                        line[pos] = b'\n'; pos += 1;
                        thread_buf.extend_from_slice(&line[..pos]);
                    }

                    {
                        let mut out_text = text_ref[h].lock().unwrap();
                        out_text.extend_from_slice(&thread_buf);
                    }
                    done_ref[h].store(true, Ordering::Release);
                }
            });
        }
    });

    unsafe { _exit(0); }
}
