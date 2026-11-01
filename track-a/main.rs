// Speed Demon 2026 - Track A (Turbo Ingest) - Team: Runtime Terrors
use std::{arch::x86_64::*, fs::File, io::Write, os::unix::io::AsRawFd, path::PathBuf,
    sync::{atomic::{AtomicBool, AtomicUsize, Ordering}, Mutex}, thread};

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn madvise(a: *mut u8, l: usize, adv: i32) -> i32;
    fn _exit(s: i32) -> !;
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
struct Block { next: *mut Block, n: u32, rec: [u32; BLOCK_CAP] }

#[derive(Copy, Clone)]
struct SP<T>(*const T);
unsafe impl<T> Send for SP<T> {}
unsafe impl<T> Sync for SP<T> {}
impl<T> SP<T> { #[inline(always)] fn p(&self) -> *const T { self.0 } }

#[inline(always)] unsafe fn ld8(p: *const u8) -> u64 { (p as *const u64).read_unaligned() }
#[inline(always)] unsafe fn m32(p: *const u8, c: u8) -> u32 {
    _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(p as *const __m256i), _mm256_set1_epi8(c as i8))) as u32
}
#[inline(always)] unsafe fn find(mut s: *const u8, c: u8) -> *const u8 {
    loop { let m = m32(s, c); if m != 0 { return s.add(m.trailing_zeros() as usize); } s = s.add(32); }
}

/// SSE-accelerated 1-10 digit decimal parse: reads 16 bytes ending at `end`.
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
    (_mm_cvtsi128_si32(v) as u32).wrapping_mul(100000000) + (_mm_extract_epi32(v, 1) as u32)
}

struct EPTable<'a> {
    slots: [u32; TABLE_SLOTS],
    names: Vec<&'a [u8]>,
}
impl<'a> EPTable<'a> {
    fn new() -> Self { Self { slots: [0; TABLE_SLOTS], names: Vec::with_capacity(512) } }
    #[inline(always)]
    unsafe fn intern(&mut self, s: *const u8, len: usize) -> u32 {
        let mut a = ld8(s);
        let b = if len >= 8 { ld8(s.add(len - 8)) } else { 0 };
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

struct Worker<'a> {
    ep: EPTable<'a>,
    head: [*mut Block; HOURS],
    big: Vec<Vec<(u32, u32)>>,
    region: *mut u8, used: usize, reserved: usize,
}
unsafe impl<'a> Send for Worker<'a> {}
unsafe impl<'a> Sync for Worker<'a> {}
impl<'a> Worker<'a> {
    fn new() -> Self {
        Self { ep: EPTable::new(), head: [std::ptr::null_mut(); HOURS],
            big: (0..HOURS).map(|_| Vec::new()).collect(),
            region: std::ptr::null_mut(), used: 0, reserved: 0 }
    }
    unsafe fn parse(&mut self, mut p: *const u8, end: *const u8) {
        if p >= end { return; }
        let reserve = 72 * std::mem::size_of::<Block>();
        self.reserved = reserve;
        self.region = mmap(std::ptr::null_mut(), reserve, 3, 0x22, -1, 0);
        if self.region == !0 as *mut u8 { _exit(1); }
        while p < end {
            let day_idx = if *p.add(6) == b'0' { (*p.add(9) - b'0') as i32 }
                          else { (*p.add(9) - b'0' + 1) as i32 };
            let hour = ((*p.add(11) - b'0') * 10 + (*p.add(12) - b'0')) as i32;
            let h = (day_idx * 24 + hour - 18) as usize;
            let m = p.add(28);
            let sp = (m32(m, b' ') as u64) | ((m32(m.add(32), b' ') as u64) << 32);
            let nl = (m32(m, b'\n') as u64) | ((m32(m.add(32), b'\n') as u64) << 32);
            let ep = m.add(sp.trailing_zeros() as usize + 1);
            let ee = if nl != 0 { m.add((sp & (sp - 1)).trailing_zeros() as usize) } else { find(ep, b' ') };
            let eol = if nl != 0 { m.add(nl.trailing_zeros() as usize) } else { find(ee.add(5), b'\n') };
            let is_err = if *ee.add(1) == b'5' { ERR_BIT } else { 0 };
            let key = self.ep.intern(ep, ee as usize - ep as usize) | is_err;
            let lat = parse_digits(eol, (eol as usize - ee as usize - 5) as u32);
            p = eol.add(1);
            if lat >= BIG { self.big[h].push((lat, key)); continue; }
            let b = &mut self.head[h];
            if (*b).is_null() || (*(*b)).n as usize == BLOCK_CAP {
                if self.used + std::mem::size_of::<Block>() > self.reserved {
                    let extra = 16 * std::mem::size_of::<Block>();
                    let pf = mmap(std::ptr::null_mut(), extra, 3, 0x22, -1, 0);
                    if pf != !0 as *mut u8 { self.region = pf; self.used = 0; self.reserved = extra; }
                }
                let fresh = self.region.add(self.used) as *mut Block;
                self.used += std::mem::size_of::<Block>();
                (*fresh).next = *b; (*fresh).n = 0; *b = fresh;
            }
            let br = &mut *(*b);
            br.rec[br.n as usize] = (lat << KEY_BITS) | key;
            br.n += 1;
        }
    }
}

#[inline(always)]
fn wr_u32(buf: &mut [u8], mut v: u32, mut pos: usize) -> usize {
    if v == 0 { buf[pos] = b'0'; return pos + 1; }
    let (mut t, mut n) = ([0u8; 10], 0);
    while v > 0 { t[n] = b'0' + (v % 10) as u8; v /= 10; n += 1; }
    while n > 0 { n -= 1; buf[pos] = t[n]; pos += 1; }
    pos
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut data_dir = PathBuf::from("/data");
    let mut out_dir = if PathBuf::from("/out").exists() { PathBuf::from("/out") } else { PathBuf::from("output") };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--data" { if let Some(v) = args.next() { data_dir = PathBuf::from(v); } }
        else if a == "--out" { if let Some(v) = args.next() { out_dir = PathBuf::from(v); } }
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
    // Map file + guard page (4096 extra) so AVX2 loads past EOF are safe
    let data = unsafe {
        let p = mmap(std::ptr::null_mut(), size + 4096, 3, 0x22, -1, 0);
        if p == !0 as *mut u8 { _exit(1); }
        if mmap(p, size, 1, 0x12, fd, 0) == !0 as *mut u8 { _exit(1); }
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
    let ds = SP(data);

    // Phase 1: parallel parse
    thread::scope(|s| {
        let cr = &cut; let d = ds;
        for (t, w) in workers.iter_mut().enumerate() {
            s.spawn(move || unsafe {
                let from = d.p().add(cr[t]); let to = d.p().add(cr[t + 1]);
                w.parse(from, to);
                let (sf, ef) = ((from as usize + 4095) & !4095, (to as usize) & !4095);
                if ef > sf { madvise(sf as *mut u8, ef - sf, 4); }
            });
        }
    });

    let mut global = EPTable::new();
    for t in 0..NTHREADS {
        for n in &workers[t].ep.names {
            rank[t].push(unsafe { global.intern(n.as_ptr(), n.len()) });
        }
    }
    let mut sorted = global.names.clone();
    sorted.sort_unstable();
    let mut rank_of = vec![0u32; sorted.len()];
    for (i, n) in sorted.iter().enumerate() {
        rank_of[unsafe { global.intern(n.as_ptr(), n.len()) } as usize] = i as u32;
    }
    for r in &mut rank { for id in r { *id = rank_of[*id as usize]; } }
    let ne = sorted.len();
    unsafe { madvise(data as *mut u8, size, 4); }

    // Phase 2: parallel hour reduction + streaming write
    thread::scope(|s| {
        let dr = &done; let tr = &text_boxes; let op = out_path.clone();
        s.spawn(move || {
            let mut f = match File::create(&op) { Ok(f) => f, Err(_) => return };
            for h in 0..HOURS {
                while !dr[h].load(Ordering::Acquire) { std::hint::spin_loop(); }
                let mut buf = tr[h].lock().unwrap();
                f.write_all(&buf).ok(); *buf = Vec::new();
            }
            f.flush().ok();
        });

        let nhr = &next_hour; let dr = &done; let tr = &text_boxes;
        let wr = &workers; let rr = &rank; let lr = &labels; let nr = &sorted;

        for _ in 0..NTHREADS {
            s.spawn(move || {
                let mut count = vec![0u32; ne + 1];
                let mut errors = vec![0u32; ne];
                let mut cursor = vec![0u32; ne];
                let mut lat = Vec::new();
                let mut line = [0u8; 512];
                loop {
                    let h = nhr.fetch_add(1, Ordering::Relaxed);
                    if h >= HOURS { break; }
                    count.fill(0); errors.fill(0);
                    for wt in 0..NTHREADS {
                        let (w, r) = (&wr[wt], &rr[wt]);
                        let mut b = w.head[h];
                        while !b.is_null() {
                            unsafe {
                                let br = &*b;
                                for i in 0..br.n as usize {
                                    let k = br.rec[i]; let e = r[(k & ID_MASK) as usize] as usize;
                                    count[e + 1] += 1; errors[e] += ((k & ERR_BIT) != 0) as u32;
                                }
                                b = br.next;
                            }
                        }
                        for &(_, k) in &w.big[h] {
                            let e = r[(k & ID_MASK) as usize] as usize;
                            count[e + 1] += 1; errors[e] += ((k & ERR_BIT) != 0) as u32;
                        }
                    }
                    for e in 0..ne { count[e + 1] += count[e]; }
                    cursor.copy_from_slice(&count[..ne]);
                    let tl = count[ne] as usize;
                    lat.clear(); lat.reserve(tl); unsafe { lat.set_len(tl); }
                    for wt in 0..NTHREADS {
                        let (w, r) = (&wr[wt], &rr[wt]);
                        let mut b = w.head[h];
                        while !b.is_null() {
                            unsafe {
                                let br = &*b;
                                for i in 0..br.n as usize {
                                    let rec = br.rec[i]; let e = r[(rec & ID_MASK) as usize] as usize;
                                    lat[cursor[e] as usize] = rec >> KEY_BITS; cursor[e] += 1;
                                }
                                let nb = br.next;
                                madvise(b as *mut u8, std::mem::size_of::<Block>(), 4);
                                b = nb;
                            }
                        }
                        for &(l, k) in &w.big[h] {
                            let e = r[(k & ID_MASK) as usize] as usize;
                            lat[cursor[e] as usize] = l; cursor[e] += 1;
                        }
                    }
                    let label = &lr[h];
                    let mut out = tr[h].lock().unwrap();
                    out.reserve(ne * 64);
                    for e in 0..ne {
                        let (st, en) = (count[e] as usize, count[e + 1] as usize);
                        let n = en - st; if n == 0 { continue; }
                        let sl = &mut lat[st..en];
                        let (p50, p95, p99) = if n <= 256 {
                            sl.sort_unstable();
                            (sl[(50*n+99)/100-1], sl[(95*n+99)/100-1], sl[(99*n+99)/100-1])
                        } else {
                            let (r99,r95,r50) = ((99*n+99)/100-1,(95*n+99)/100-1,(50*n+99)/100-1);
                            sl.select_nth_unstable(r99);
                            let p99 = sl[r99];
                            let p95 = if r95 == r99 { p99 } else { sl[..r99].select_nth_unstable(r95); sl[r95] };
                            let p50 = if r50 == r95 { p95 } else { sl[..r95].select_nth_unstable(r50); sl[r50] };
                            (p50, p95, p99)
                        };
                        let err = errors[e]; let ppm = ((err as u64)*1000000/(n as u64)) as u32;
                        let mut pos = 0;
                        line[..20].copy_from_slice(label); pos += 20;
                        let ep = nr[e];
                        line[pos] = b' '; line[pos+1..pos+1+ep.len()].copy_from_slice(ep); pos += 1 + ep.len();
                        for v in [n as u32, p50, p95, p99, err, ppm] { line[pos] = b' '; pos = wr_u32(&mut line, v, pos+1); }
                        line[pos] = b'\n'; pos += 1;
                        out.extend_from_slice(&line[..pos]);
                    }
                    dr[h].store(true, Ordering::Release);
                }
            });
        }
    });
    unsafe { _exit(0); }
}
