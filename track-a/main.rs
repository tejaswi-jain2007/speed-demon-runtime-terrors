// Speed Demon 2026 - Track A (Turbo Ingest) - Team: Runtime Terrors
use std::{arch::x86_64::*, fs::File, io::Write, os::unix::io::AsRawFd, path::PathBuf, sync::{atomic::{AtomicBool, AtomicUsize, Ordering}, Mutex}, thread};

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn madvise(a: *mut u8, l: usize, adv: i32) -> i32;
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

#[repr(C)]
struct Block {
    next: *mut Block,
    n: u32,
    rec: [u32; BLOCK_CAP],
}

#[derive(Copy, Clone)]
struct SendPtr<T>(*const T);
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}
impl<T> SendPtr<T> { #[inline(always)] fn as_ptr(&self) -> *const T { self.0 } }

#[inline(always)] unsafe fn load8(p: *const u8) -> u64 { (p as *const u64).read_unaligned() }
#[inline(always)]
unsafe fn find(mut s: *const u8, c: u8) -> *const u8 {
    loop {
        let v = _mm256_loadu_si256(s as *const __m256i);
        let m = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(c as i8))) as u32;
        if m != 0 { return s.add(m.trailing_zeros() as usize); }
        s = s.add(32);
    }
}

struct EndpointTable<'a> {
    slots: [u32; TABLE_SLOTS],
    fp1: [u64; TABLE_SLOTS],
    fp2: [u64; TABLE_SLOTS],
    names: Vec<&'a [u8]>,
}

impl<'a> EndpointTable<'a> {
    fn new() -> Self {
        Self {
            slots: [0; TABLE_SLOTS],
            fp1: [0; TABLE_SLOTS],
            fp2: [0; TABLE_SLOTS],
            names: Vec::with_capacity(512),
        }
    }

    #[inline(always)]
    unsafe fn intern(&mut self, s: *const u8, len: usize) -> u32 {
        let mut a = load8(s);
        let b = if len >= 8 { load8(s.add(len - 8)) } else { 0 };
        let c = if len >= 16 { load8(s.add(8)) } else { 0 };
        let d = if len >= 24 { load8(s.add(len - 16)) } else { 0 };
        if len < 8 { a &= (1u64 << (8 * len)) - 1; }
        let h1 = _mm_crc32_u64(len as u64, a ^ d);
        let h2 = _mm_crc32_u64(h1, b ^ c);
        let fp1 = ((len as u64) << 32) | (h2 as u32 as u64);
        let fp2 = a ^ b ^ c ^ d;
        let mut i = h2 as usize;
        loop {
            let slot = self.slots[i & (TABLE_SLOTS - 1)];
            if slot == 0 {
                let idx = self.names.len() as u32;
                self.names.push(std::slice::from_raw_parts(s, len));
                self.slots[i & (TABLE_SLOTS - 1)] = idx + 1;
                self.fp1[i & (TABLE_SLOTS - 1)] = fp1;
                self.fp2[i & (TABLE_SLOTS - 1)] = fp2;
                return idx;
            }
            if self.fp1[i & (TABLE_SLOTS - 1)] == fp1 && self.fp2[i & (TABLE_SLOTS - 1)] == fp2 {
                if len <= 32 { return slot - 1; }
                let n = self.names[(slot - 1) as usize];
                if std::slice::from_raw_parts(s, len) == n { return slot - 1; }
            }
            i += 1;
        }
    }
}

struct Worker<'a> {
    endpoints: EndpointTable<'a>,
    head: [*mut Block; HOURS],
    big: Vec<(usize, u32, u32)>,
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
            big: Vec::with_capacity(32),
            region: std::ptr::null_mut(),
            used: 0,
            reserved: 0,
        }
    }

    unsafe fn parse(&mut self, mut p: *const u8, end: *const u8) {
        if p >= end { return; }
        let reserve = 72 * std::mem::size_of::<Block>();
        self.reserved = reserve;
        self.region = mmap(std::ptr::null_mut(), reserve, 3, 0x22, -1, 0);
        if self.region == !0 as *mut u8 { _exit(1); }

        while p < end {
            let day_idx = (*p.add(9) - b'0' + (*p.add(6) & 1)) as usize;
            let hour = ((*p.add(11) - b'0') * 10 + (*p.add(12) - b'0')) as usize;
            let h = day_idx * 24 + hour - 18;

            let ep = p.add(if *p.add(31) == b' ' { 32 } else if *p.add(32) == b' ' { 33 } else if *p.add(33) == b' ' { 34 } else if *p.add(34) == b' ' { 35 } else { 36 });

            let v = _mm256_loadu_si256(ep as *const __m256i);
            let m1 = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(b' ' as i8))) as u32;
            let ee = if m1 != 0 { ep.add(m1.trailing_zeros() as usize) } else { find(ep.add(32), b' ') };

            let is_err = ((*ee.add(1) == b'5') as u32) << 11;
            let key = self.endpoints.intern(ep, ee as usize - ep as usize) | is_err;

            let mut cur = ee.add(5);
            let mut lat = (*cur - b'0') as u32;
            cur = cur.add(1);
            while *cur >= b'0' {
                lat = lat * 10 + (*cur - b'0') as u32;
                cur = cur.add(1);
            }
            p = if *cur == b'\r' { cur.add(2) } else { cur.add(1) };

            if lat >= BIG {
                self.big.push((h, lat, key));
                continue;
            }

            let b = &mut self.head[h];
            if (*b).is_null() || (*(*b)).n as usize == BLOCK_CAP {
                if self.used + std::mem::size_of::<Block>() > self.reserved {
                    let extra = 16 * std::mem::size_of::<Block>();
                    let p_fresh = mmap(std::ptr::null_mut(), extra, 3, 0x22, -1, 0);
                    if p_fresh != !0 as *mut u8 {
                        self.region = p_fresh;
                        self.used = 0;
                        self.reserved = extra;
                    }
                }
                let fresh = self.region.add(self.used) as *mut Block;
                self.used += std::mem::size_of::<Block>();
                (*fresh).next = *b;
                (*fresh).n = 0;
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut data_dir = PathBuf::from("/data");
    let mut out_dir = if PathBuf::from("/out").exists() { PathBuf::from("/out") } else { PathBuf::from("output") };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--data" { if let Some(v) = args.next() { data_dir = PathBuf::from(v); } }
        else if arg == "--out" { if let Some(v) = args.next() { out_dir = PathBuf::from(v); } }
    }
    if let Ok(v) = std::env::var("DATA_DIR") { if !v.is_empty() { data_dir = PathBuf::from(v); } }
    if let Ok(v) = std::env::var("OUT_DIR") { if !v.is_empty() { out_dir = PathBuf::from(v); } }
    std::fs::create_dir_all(&out_dir).ok();

    let in_path = if data_dir.is_file() { data_dir } else { data_dir.join("requests.log") };
    let out_path = out_dir.join("summary.txt");
    if !in_path.exists() { File::create(&out_path)?; return Ok(()); }
    let file = File::open(&in_path)?;
    let size = file.metadata()?.len() as usize;
    if size == 0 { File::create(&out_path)?; return Ok(()); }

    let fd = file.as_raw_fd();
    let data = unsafe {
        let p = mmap(std::ptr::null_mut(), size, 1, 2, fd, 0);
        if p == !0 as *mut u8 { _exit(1); }
        madvise(p, size, 2);
        p as *const u8
    };

    let mut labels = [[0u8; 20]; HOURS];
    for h in 0..HOURS {
        let (day, hr) = ((h + 18) / 24, (h + 18) % 24);
        let (m, d) = if day < 2 { (10, 30 + day) } else { (11, day - 1) };
        labels[h].copy_from_slice(format!("2026-{m:02}-{d:02}T{hr:02}:00:00Z").as_bytes());
    }

    let mut cut = vec![size; NTHREADS + 1]; cut[0] = 0;
    for t in 1..NTHREADS {
        let mut c = cut[t - 1].max(size * t / NTHREADS);
        while c < size && unsafe { *data.add(c - 1) } != b'\n' { c += 1; }
        cut[t] = c;
    }

    let mut workers: Vec<Worker> = (0..NTHREADS).map(|_| Worker::new()).collect();
    let next_hour = AtomicUsize::new(0);
    let done: Vec<AtomicBool> = (0..HOURS).map(|_| AtomicBool::new(false)).collect();
    let text_boxes: Vec<Mutex<Vec<u8>>> = (0..HOURS).map(|_| Mutex::new(Vec::new())).collect();
    let mut rank = vec![Vec::new(); NTHREADS];
    let data_send = SendPtr(data);

    // Phase 1: parse chunks in parallel
    thread::scope(|s| {
        let cut_ref = &cut;
        let d_send = data_send;
        for (t, w) in workers.iter_mut().enumerate() {
            s.spawn(move || unsafe {
                let from = d_send.as_ptr().add(cut_ref[t]);
                let to = d_send.as_ptr().add(cut_ref[t + 1]);
                w.parse(from, to);
                let sf = (from as usize + 4095) & !4095;
                let ef = (to as usize) & !4095;
                if ef > sf { madvise(sf as *mut u8, ef - sf, 4); }
            });
        }
    });

    let mut global = EndpointTable::new();
    for t in 0..NTHREADS {
        for n in &workers[t].endpoints.names {
            rank[t].push(unsafe { global.intern(n.as_ptr(), n.len()) });
        }
    }
    let mut sorted_names = global.names.clone();
    sorted_names.sort_unstable();

    let mut rank_of = vec![0u32; sorted_names.len()];
    for (i, n) in sorted_names.iter().enumerate() {
        let global_id = unsafe { global.intern(n.as_ptr(), n.len()) } as usize;
        rank_of[global_id] = i as u32;
    }
    for r in &mut rank {
        for id in r { *id = rank_of[*id as usize]; }
    }
    let num_endpoints = sorted_names.len();
    unsafe { madvise(data as *mut u8, size, 4); }

    // Phase 2: parallel hour reduction and streaming writer
    thread::scope(|s| {
        let done_ref = &done;
        let text_ref = &text_boxes;
        let out_p = out_path.clone();
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

        let next_h_ref = &next_hour;
        let done_ref = &done;
        let text_ref = &text_boxes;
        let workers_ref = &workers;
        let rank_ref = &rank;
        let labels_ref = &labels;
        let names_ref = &sorted_names;
        let ne = num_endpoints;

        for _ in 0..NTHREADS {
            s.spawn(move || {
                let mut count = vec![0u32; ne + 1];
                let mut errors = vec![0u32; ne];
                let mut cursor = vec![0u32; ne];
                let mut lat = Vec::new();
                let mut line = [0u8; 512];

                loop {
                    let h = next_h_ref.fetch_add(1, Ordering::Relaxed);
                    if h >= HOURS { break; }
                    count.fill(0); errors.fill(0);

                    for wt in 0..NTHREADS {
                        let w = &workers_ref[wt];
                        let r = &rank_ref[wt];
                        let mut b = w.head[h];
                        while !b.is_null() {
                            unsafe {
                                let b_ref = &*b;
                                for i in 0..b_ref.n as usize {
                                    let key = b_ref.rec[i];
                                    let e = r[(key & ID_MASK) as usize] as usize;
                                    count[e + 1] += 1;
                                    errors[e] += ((key & ERR_BIT) != 0) as u32;
                                }
                                b = b_ref.next;
                            }
                        }
                        for &(big_h, _l, key) in &w.big {
                            if big_h == h {
                                let e = r[(key & ID_MASK) as usize] as usize;
                                count[e + 1] += 1;
                                errors[e] += ((key & ERR_BIT) != 0) as u32;
                            }
                        }
                    }

                    for e in 0..ne { count[e + 1] += count[e]; }
                    cursor.copy_from_slice(&count[..ne]);
                    let total_lat = count[ne] as usize;
                    lat.clear();
                    lat.reserve(total_lat);
                    unsafe { lat.set_len(total_lat); }

                    for wt in 0..NTHREADS {
                        let w = &workers_ref[wt];
                        let r = &rank_ref[wt];
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
                                madvise(b as *mut u8, std::mem::size_of::<Block>(), 4);
                                b = next_b;
                            }
                        }
                        for &(big_h, l, key) in &w.big {
                            if big_h == h {
                                let e = r[(key & ID_MASK) as usize] as usize;
                                lat[cursor[e] as usize] = l;
                                cursor[e] += 1;
                            }
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
                        let (p50, p95, p99) = if n <= 256 {
                            slice.sort_unstable();
                            (slice[(50 * n + 99) / 100 - 1], slice[(95 * n + 99) / 100 - 1], slice[(99 * n + 99) / 100 - 1])
                        } else {
                            let (r99, r95, r50) = ((99 * n + 99) / 100 - 1, (95 * n + 99) / 100 - 1, (50 * n + 99) / 100 - 1);
                            slice.select_nth_unstable(r99);
                            slice[..r99].select_nth_unstable(r95);
                            slice[..r95].select_nth_unstable(r50);
                            (slice[r50], slice[r95], slice[r99])
                        };

                        let err = errors[e];
                        let ppm = ((err as u64) * 1000000 / (n as u64)) as u32;

                        let mut pos = 0;
                        line[..20].copy_from_slice(label); pos += 20;

                        let ep = names_ref[e];
                        line[pos] = b' ';
                        line[pos + 1..pos + 1 + ep.len()].copy_from_slice(ep);
                        pos += 1 + ep.len();

                        for v in [n as u32, p50, p95, p99, err, ppm] {
                            line[pos] = b' ';
                            pos = append_u32(&mut line, v, pos + 1);
                        }
                        line[pos] = b'\n';
                        pos += 1;

                        out_text.extend_from_slice(&line[..pos]);
                    }
                    done_ref[h].store(true, Ordering::Release);
                }
            });
        }
    });

    unsafe { _exit(0); }
}
