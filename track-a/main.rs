// Speed Demon 2026 - Track A (Ingest) - Single Scope Architecture Test
use std::arch::x86_64::*;
use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Barrier;
use std::thread;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    fn prctl(option: i32, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> i32;
}

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_PRIVATE: i32 = 0x02;
const MAP_ANONYMOUS: i32 = 0x20;
const MAP_FIXED: i32 = 0x10;
const MAP_FAILED: *mut u8 = !0 as *mut u8;
const MADV_DONTNEED: i32 = 4;

const HOURS: usize = 72;
const KEY_BITS: u32 = 12;
const ID_MASK: u32 = (1 << 11) - 1;
const ERR_BIT: u32 = 1 << 11;
const BIG: u32 = 1 << 20;
const NTHREADS: usize = 12;
const TABLE_SLOTS: usize = 1 << 12;

#[inline(always)]
unsafe fn load8(p: *const u8) -> u64 {
    (p as *const u64).read_unaligned()
}

#[inline(always)]
unsafe fn match32(p: *const u8, c: u8) -> u32 {
    let v = _mm256_loadu_si256(p as *const __m256i);
    _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(c as i8))) as u32
}

#[inline(always)]
unsafe fn find(mut s: *const u8, c: u8) -> *const u8 {
    loop {
        let m = match32(s, c);
        if m != 0 {
            return s.add(m.trailing_zeros() as usize);
        }
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
    let high = _mm_cvtsi128_si32(v) as u32;
    let low = _mm_extract_epi32(v, 1) as u32;
    high.wrapping_mul(100000000) + low
}

struct SendPtr<T>(*const T);
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}
impl<T> Clone for SendPtr<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for SendPtr<T> {}
impl<T> SendPtr<T> {
    #[inline(always)]
    fn as_ptr(&self) -> *const T {
        self.0
    }
}

struct SendMutPtr<T>(*mut T);
unsafe impl<T> Send for SendMutPtr<T> {}
unsafe impl<T> Sync for SendMutPtr<T> {}
impl<T> Clone for SendMutPtr<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for SendMutPtr<T> {}
impl<T> SendMutPtr<T> {
    #[inline(always)]
    fn as_ptr(&self) -> *mut T {
        self.0
    }
}

struct EndpointTable<'a> {
    slots: [u32; TABLE_SLOTS],
    names: Vec<&'a [u8]>,
}

impl<'a> EndpointTable<'a> {
    fn new() -> Self {
        Self {
            slots: [0; TABLE_SLOTS],
            names: Vec::with_capacity(512),
        }
    }

    #[inline(always)]
    unsafe fn intern(&mut self, s: *const u8, len: usize) -> u32 {
        let mut a = load8(s);
        let b = if len >= 8 { load8(s.add(len - 8)) } else { 0 };
        if len < 8 {
            a &= (1u64 << (8 * len)) - 1;
        }
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
            if n.len() == len {
                if len < 8 {
                    if std::slice::from_raw_parts(s, len) == n {
                        return *slot - 1;
                    }
                } else if load8(n.as_ptr()) == a && load8(n.as_ptr().add(len - 8)) == b {
                    if len <= 16 || std::slice::from_raw_parts(s, len) == n {
                        return *slot - 1;
                    }
                }
            }
            i += 1;
        }
    }
}

const BLOCK_CAP: usize = (16384 - 16) / 4;

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

    #[inline(always)]
    unsafe fn push(&mut self, h: usize, rec: u32) {
        let b = &mut self.head[h];
        if (*b).is_null() || (*(*b)).n as usize == BLOCK_CAP {
            if self.used + std::mem::size_of::<Block>() > self.reserved {
                let extra = 16 * std::mem::size_of::<Block>();
                let p = mmap(std::ptr::null_mut(), extra, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
                if p != MAP_FAILED { self.region = p; self.used = 0; self.reserved = extra; }
            }
            let fresh = self.region.add(self.used) as *mut Block;
            self.used += std::mem::size_of::<Block>();
            (*fresh).next = *b;
            (*fresh).n = 0;
            *b = fresh;
        }
        let b_ref = &mut *(*b);
        b_ref.rec[b_ref.n as usize] = rec;
        b_ref.n += 1;
    }

    unsafe fn parse(&mut self, mut p: *const u8, end: *const u8) {
        if p >= end {
            return;
        }
        let reserve = 72 * std::mem::size_of::<Block>();
        self.reserved = reserve;
        self.region = mmap(
            std::ptr::null_mut(),
            reserve,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            -1,
            0,
        );
        if self.region == MAP_FAILED {
            std::process::exit(1);
        }

        let mut prev_tag = 0u32;
        let mut cur_h = 0usize;

        while p < end {
            let tag = (p.add(9) as *const u32).read_unaligned();
            if tag != prev_tag {
                prev_tag = tag;
                let day_idx = if *p.add(6) == b'0' {
                    (*p.add(9) - b'0') as i32
                } else {
                    (*p.add(9) - b'0' + 1) as i32
                };
                let hour = ((*p.add(11) - b'0') * 10 + (*p.add(12) - b'0')) as i32;
                cur_h = (day_idx * 24 + hour - 18) as usize;
            }
            let h = cur_h;

            let m = p.add(28);
            let sp = (match32(m, b' ') as u64) | ((match32(m.add(32), b' ') as u64) << 32);
            let nl = (match32(m, b'\n') as u64) | ((match32(m.add(32), b'\n') as u64) << 32);
            let ep = m.add(sp.trailing_zeros() as usize + 1);
            let ee = if nl != 0 {
                m.add((sp & (sp - 1)).trailing_zeros() as usize)
            } else {
                find(ep, b' ')
            };
            let eol = if nl != 0 {
                m.add(nl.trailing_zeros() as usize)
            } else {
                find(ee.add(5), b'\n')
            };

            let is_err = if *ee.add(1) == b'5' { ERR_BIT } else { 0 };
            let key = self.endpoints.intern(ep, ee as usize - ep as usize) | is_err;
            let lat = parse_digits(eol, (eol as usize - ee as usize - 5) as u32);
            p = eol.add(1);

            if lat >= BIG {
                self.big[h].push((lat, key));
            } else {
                self.push(h, (lat << KEY_BITS) | key);
            }
        }
    }
}

static DIGITS: &[u8; 200] = b"\
0001020304050607080910111213141516171819\
2021222324252627282930313233343536373839\
4041424344454647484950515253545556575859\
6061626364656667686970717273747576777879\
8081828384858687888990919293949596979899";

#[inline(always)]
unsafe fn append_u32(buf: *mut u8, mut val: u32) -> usize {
    if val < 10 {
        *buf = b'0' + val as u8;
        return 1;
    }
    let mut temp = [0u8; 10];
    let mut i = 10;
    while val >= 100 {
        let q = val / 100;
        let r = (val - q * 100) as usize;
        val = q;
        i -= 2;
        let d = &DIGITS[r * 2..r * 2 + 2];
        temp[i] = d[0];
        temp[i + 1] = d[1];
    }
    if val >= 10 {
        i -= 2;
        let d = &DIGITS[val as usize * 2..val as usize * 2 + 2];
        temp[i] = d[0];
        temp[i + 1] = d[1];
    } else {
        i -= 1;
        temp[i] = b'0' + val as u8;
    }
    let len = 10 - i;
    std::ptr::copy_nonoverlapping(temp.as_ptr().add(i), buf, len);
    len
}

struct SyncCell<T>(std::cell::UnsafeCell<T>);
unsafe impl<T> Sync for SyncCell<T> {}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        prctl(14, 1, 0, 0, 0);
    }
    let (mut d_dir, mut o_dir) = (String::from("/data"), String::from("/out"));
    let args: Vec<String> = std::env::args().collect();
    let (mut ai, mut pos) = (1, 0);
    while ai < args.len() {
        if args[ai] == "--data" && ai + 1 < args.len() {
            d_dir = args[ai + 1].clone();
            ai += 2;
        } else if args[ai] == "--out" && ai + 1 < args.len() {
            o_dir = args[ai + 1].clone();
            ai += 2;
        } else if !args[ai].starts_with("--") {
            if pos == 0 {
                d_dir = args[ai].clone();
            } else if pos == 1 {
                o_dir = args[ai].clone();
            }
            pos += 1;
            ai += 1;
        } else {
            ai += 1;
        }
    }
    std::fs::create_dir_all(&o_dir).ok();
    let in_path = if std::path::Path::new(&d_dir).is_file() {
        d_dir
    } else {
        format!("{d_dir}/requests.log")
    };
    let out_path = format!("{o_dir}/summary.txt");
    let file = match File::open(&in_path) {
        Ok(f) => f,
        Err(_) => {
            File::create(&out_path)?;
            return Ok(());
        }
    };
    let size = file.metadata()?.len() as usize;
    if size == 0 {
        File::create(&out_path)?;
        return Ok(());
    }

    let fd = file.as_raw_fd();
    let data = unsafe {
        let p = mmap(
            std::ptr::null_mut(),
            size + 4096,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            -1,
            0,
        );
        if p == MAP_FAILED {
            std::process::exit(1);
        }
        if mmap(p, size, PROT_READ, MAP_PRIVATE | MAP_FIXED, fd, 0) == MAP_FAILED {
            std::process::exit(1);
        }
        p as *const u8
    };

    let mut labels = [[0u8; 20]; HOURS];
    for h in 0..HOURS {
        let day = (h + 18) / 24;
        let m = if day < 2 { 10 } else { 11 };
        let d = if day < 2 { 30 + day } else { day - 1 };
        let hr = (h + 18) % 24;
        labels[h].copy_from_slice(format!("2026-{m:02}-{d:02}T{hr:02}:00:00Z").as_bytes());
    }

    let mut cut = vec![size; NTHREADS + 1];
    cut[0] = 0;
    for t in 1..NTHREADS {
        let mut c = cut[t - 1].max(size * t / NTHREADS);
        while c < size && unsafe { *data.add(c - 1) } != b'\n' {
            c += 1;
        }
        cut[t] = c;
    }

    let mut workers: Vec<Worker> = (0..NTHREADS).map(|_| Worker::new()).collect();
    let b1 = Barrier::new(NTHREADS);
    let b2 = Barrier::new(NTHREADS);
    let b_start = Barrier::new(NTHREADS + 1);

    let next_hour = AtomicUsize::new(0);
    let done: Vec<AtomicBool> = (0..HOURS).map(|_| AtomicBool::new(false)).collect();
    let text_boxes: Vec<SyncCell<Vec<u8>>> = (0..HOURS)
        .map(|_| SyncCell(std::cell::UnsafeCell::new(Vec::new())))
        .collect();
    let shared_rank: SyncCell<Vec<Vec<u32>>> = SyncCell(std::cell::UnsafeCell::new(Vec::new()));
    let shared_names: SyncCell<Vec<&[u8]>> = SyncCell(std::cell::UnsafeCell::new(Vec::new()));

    let data_send = SendPtr(data);
    let wrk_raw = SendMutPtr(workers.as_mut_ptr());

    thread::scope(|s| {
        // Background writer thread
        let done_ref = &done;
        let text_ref = &text_boxes;
        let b_start_ref = &b_start;
        let out_p = out_path.clone();
        s.spawn(move || {
            let mut out_file = match File::create(&out_p) {
                Ok(f) => f,
                Err(_) => return,
            };
            b_start_ref.wait(); // Wait until Phase 2 starts!
            for h in 0..HOURS {
                while !done_ref[h].load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                let buf = unsafe { &mut *text_ref[h].0.get() };
                out_file.write_all(buf).ok();
                buf.clear();
                buf.shrink_to_fit();
            }
            out_file.flush().ok();
        });

        for t in 0..NTHREADS {
            let cut_ref = &cut;
            let b1_ref = &b1;
            let b2_ref = &b2;
            let b_start_ref = &b_start;
            let d_send = data_send;
            let w_ptr = wrk_raw;
            let s_rank = &shared_rank;
            let s_names = &shared_names;
            let next_h_ref = &next_hour;
            let done_ref = &done;
            let text_ref = &text_boxes;
            let labels_ref = &labels;

            s.spawn(move || {
                // PHASE 1: Parse chunk
                unsafe {
                    let w = &mut *w_ptr.as_ptr().add(t);
                    let from = d_send.as_ptr().add(cut_ref[t]);
                    let to = d_send.as_ptr().add(cut_ref[t + 1]);
                    w.parse(from, to);
                }

                b1_ref.wait();

                // Thread 0 reconciles endpoints
                if t == 0 {
                    let mut global = EndpointTable::new();
                    let mut rank = vec![Vec::new(); NTHREADS];
                    for wt in 0..NTHREADS {
                        let w = unsafe { &*w_ptr.as_ptr().add(wt) };
                        for n in &w.endpoints.names {
                            rank[wt].push(unsafe { global.intern(n.as_ptr(), n.len()) });
                        }
                    }
                    let mut sorted_with_id: Vec<(u16, &[u8])> = global
                        .names
                        .iter()
                        .enumerate()
                        .map(|(i, &n)| (i as u16, n))
                        .collect();
                    sorted_with_id.sort_unstable_by_key(|&(_, n)| n);
                    let mut rank_of = vec![0u32; sorted_with_id.len()];
                    for (new_rank, &(orig_id, _)) in sorted_with_id.iter().enumerate() {
                        rank_of[orig_id as usize] = new_rank as u32;
                    }
                    for r in &mut rank {
                        for id in r {
                            *id = rank_of[*id as usize];
                        }
                    }
                    unsafe {
                        *s_rank.0.get() = rank;
                        *s_names.0.get() = sorted_with_id.iter().map(|&(_, n)| n).collect();
                        madvise(data_send.as_ptr() as *mut u8, size + 4096, MADV_DONTNEED);
                    }
                }

                b2_ref.wait();
                if t == 0 {
                    b_start_ref.wait();
                } else {
                    b_start_ref.wait();
                }

                let rank = unsafe { &*s_rank.0.get() };
                let names = unsafe { &*s_names.0.get() };
                let ne = names.len();

                let mut count = vec![0u32; ne + 1];
                let mut errors = vec![0u32; ne];
                let mut cursor = vec![0u32; ne];
                let mut lat = Vec::new();
                let mut line = [0u8; 512];

                loop {
                    let h = next_h_ref.fetch_add(1, Ordering::Relaxed);
                    if h >= HOURS {
                        break;
                    }

                    count.fill(0);
                    errors.fill(0);

                    for wt in 0..NTHREADS {
                        let w = unsafe { &*w_ptr.as_ptr().add(wt) };
                        let r = &rank[wt];
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
                        for &(l, key) in &w.big[h] {
                            let _ = l;
                            let e = r[(key & ID_MASK) as usize] as usize;
                            count[e + 1] += 1;
                            errors[e] += ((key & ERR_BIT) != 0) as u32;
                        }
                    }

                    for e in 0..ne {
                        count[e + 1] += count[e];
                    }
                    cursor.copy_from_slice(&count[..ne]);
                    let total_lat = count[ne] as usize;
                    lat.clear();
                    lat.reserve(total_lat);
                    unsafe {
                        lat.set_len(total_lat);
                    }

                    for wt in 0..NTHREADS {
                        let w = unsafe { &*w_ptr.as_ptr().add(wt) };
                        let r = &rank[wt];
                        let mut b = w.head[h];
                        while !b.is_null() {
                            unsafe {
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
                        }
                        for &(l, key) in &w.big[h] {
                            let e = r[(key & ID_MASK) as usize] as usize;
                            let dst = cursor[e] as usize;
                            lat[dst] = l;
                            cursor[e] += 1;
                        }
                    }

                    let label = &labels_ref[h];
                    let out_text = unsafe { &mut *text_ref[h].0.get() };
                    out_text.reserve(ne * 64);

                    for e in 0..ne {
                        let start = count[e] as usize;
                        let end = count[e + 1] as usize;
                        let n = end - start;
                        if n == 0 {
                            continue;
                        }

                        let slice = &mut lat[start..end];
                        let (p50, p95, p99) = if n <= 256 {
                            slice.sort_unstable();
                            (slice[(50 * n + 99) / 100 - 1], slice[(95 * n + 99) / 100 - 1], slice[(99 * n + 99) / 100 - 1])
                        } else {
                            let (r50, r95, r99) = ((50 * n + 99) / 100 - 1, (95 * n + 99) / 100 - 1, (99 * n + 99) / 100 - 1);
                            slice.select_nth_unstable(r99);
                            let p99 = slice[r99];
                            let p95 = if r95 == r99 { p99 } else { slice[..r99].select_nth_unstable(r95); slice[r95] };
                            let p50 = if r50 == r95 { p95 } else { slice[..r95].select_nth_unstable(r50); slice[r50] };
                            (p50, p95, p99)
                        };

                        let err = errors[e];
                        let ppm = ((err as u64) * 1000000 / (n as u64)) as u32;

                        let name = names[e];
                        unsafe {
                            let lp = line.as_mut_ptr();
                            std::ptr::copy_nonoverlapping(label.as_ptr(), lp, 20);
                            *lp.add(20) = b' ';
                            let mut pos = 21;
                            std::ptr::copy_nonoverlapping(name.as_ptr(), lp.add(pos), name.len());
                            pos += name.len();
                            for val in [n as u32, p50, p95, p99, err, ppm] {
                                *lp.add(pos) = b' ';
                                pos += 1 + append_u32(lp.add(pos + 1), val);
                            }
                            *lp.add(pos) = b'\n';
                            pos += 1;
                            out_text.extend_from_slice(&line[..pos]);
                        }
                    }
                    done_ref[h].store(true, Ordering::Release);
                }
            });
        }
    });

    Ok(())
}
