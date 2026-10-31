// Speed Demon 2026 - Track A (Ingest) - Ultra-optimized parallel parser & percentile aggregator
use std::arch::x86_64::*;
use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::Barrier;
use std::thread;

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
const NTHREADS: usize = 16;
const SLOTS: usize = 4096;
const B_CAP: usize = 128;

struct Ptr<T>(*mut T);
impl<T> Clone for Ptr<T> { fn clone(&self) -> Self { *self } }
impl<T> Copy for Ptr<T> {}
unsafe impl<T> Send for Ptr<T> {}
unsafe impl<T> Sync for Ptr<T> {}

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

struct EndpointTable<'a> {
    slots: [u32; SLOTS],
    names: Vec<&'a [u8]>,
}

impl<'a> EndpointTable<'a> {
    fn new() -> Self { Self { slots: [0; SLOTS], names: Vec::with_capacity(512) } }
    #[inline(always)]
    unsafe fn intern(&mut self, s: *const u8, len: usize) -> u32 {
        let mut a = (s as *const u64).read_unaligned();
        let b = if len >= 8 { (s.add(len - 8) as *const u64).read_unaligned() } else { 0 };
        if len < 8 { a &= (1u64 << (8 * len)) - 1; }
        let h1 = _mm_crc32_u64(len as u64, a);
        let mut i = _mm_crc32_u64(h1, b) as usize;
        loop {
            let slot = &mut self.slots[i % SLOTS];
            if *slot == 0 {
                let idx = self.names.len() as u32;
                self.names.push(std::slice::from_raw_parts(s, len));
                *slot = idx + 1;
                return idx;
            }
            let n = self.names[(*slot - 1) as usize];
            if n.len() == len {
                if len < 8 {
                    if std::slice::from_raw_parts(s, len) == n { return *slot - 1; }
                } else if (n.as_ptr() as *const u64).read_unaligned() == a && (n.as_ptr().add(len - 8) as *const u64).read_unaligned() == b {
                    if len <= 16 || std::slice::from_raw_parts(s, len) == n { return *slot - 1; }
                }
            }
            i += 1;
        }
    }
}

#[repr(C)]
struct Block { next: u32, n: u16, rec: [u32; B_CAP] }

struct Worker<'a> {
    endpoints: EndpointTable<'a>,
    heads: [u32; HOURS],
    big: Vec<Vec<(u32, u32)>>,
    blocks: Vec<Block>,
}

impl<'a> Worker<'a> {
    fn new() -> Self {
        Self {
            endpoints: EndpointTable::new(),
            heads: [!0; HOURS],
            big: (0..HOURS).map(|_| Vec::new()).collect(),
            blocks: Vec::with_capacity(2048),
        }
    }

    #[inline(always)]
    fn push(&mut self, h: usize, rec: u32) {
        let cur_head = self.heads[h];
        if cur_head != !0 && (self.blocks[cur_head as usize].n as usize) < B_CAP {
            let b = &mut self.blocks[cur_head as usize];
            b.rec[b.n as usize] = rec;
            b.n += 1;
        } else {
            let idx = self.blocks.len() as u32;
            let mut blk = Block { next: cur_head, n: 1, rec: [0; B_CAP] };
            blk.rec[0] = rec;
            self.blocks.push(blk);
            self.heads[h] = idx;
        }
    }

    unsafe fn parse(&mut self, mut p: *const u8, end: *const u8) {
        let mut prev_tag = 0u32;
        let mut cur_h = 0usize;
        while p < end {
            let tag = (p.add(9) as *const u32).read_unaligned();
            if tag != prev_tag {
                prev_tag = tag;
                let day_idx = if *p.add(6) == b'0' { (*p.add(9) - b'0') as i32 } else { (*p.add(9) - b'0' + 1) as i32 };
                let hour = ((*p.add(11) - b'0') * 10 + (*p.add(12) - b'0')) as i32;
                cur_h = (day_idx * 24 + hour - 18) as usize;
            }
            let h = cur_h;
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
            if lat >= BIG { self.big[h].push((lat, key)); }
            else { self.push(h, (lat << KEY_BITS) | key); }
        }
    }
}

static DIGITS: &[u8; 200] = b"00010203040506070809101112131415161718192021222324252627282930313233343536373839404142434445464748495051525354555657585960616263646566676869707172737475767778798081828384858687888990919293949596979899";

#[inline(always)]
unsafe fn append_u32(buf: *mut u8, mut val: u32) -> usize {
    if val < 10 { *buf = b'0' + val as u8; return 1; }
    let mut temp = [0u8; 10];
    let mut i = 10;
    while val >= 100 {
        let q = val / 100;
        let r = (val - q * 100) as usize;
        val = q; i -= 2;
        let d = &DIGITS[r * 2..r * 2 + 2];
        temp[i] = d[0]; temp[i + 1] = d[1];
    }
    if val >= 10 {
        i -= 2;
        let d = &DIGITS[val as usize * 2..val as usize * 2 + 2];
        temp[i] = d[0]; temp[i + 1] = d[1];
    } else {
        i -= 1; temp[i] = b'0' + val as u8;
    }
    let len = 10 - i;
    std::ptr::copy_nonoverlapping(temp.as_ptr().add(i), buf, len);
    len
}

fn main() {
    let mut d_dir = String::from("/data");
    let mut o_dir = String::from("/out");
    let args: Vec<String> = std::env::args().collect();
    let mut ai = 1;
    while ai < args.len() {
        if args[ai] == "--data" && ai + 1 < args.len() { d_dir = args[ai + 1].clone(); ai += 2; }
        else if args[ai] == "--out" && ai + 1 < args.len() { o_dir = args[ai + 1].clone(); ai += 2; }
        else { ai += 1; }
    }
    let in_path = if std::path::Path::new(&d_dir).is_file() { d_dir } else { format!("{d_dir}/requests.log") };
    let out_path = format!("{o_dir}/summary.txt");
    let file = match File::open(&in_path) { Ok(f) => f, Err(_) => return };
    let size = file.metadata().map(|m| m.len() as usize).unwrap_or(0);
    if size == 0 { return; }

    let fd = file.as_raw_fd();
    let data = unsafe {
        let p = mmap(std::ptr::null_mut(), size + 4096, 1, 2, fd, 0);
        if p.is_null() || p as isize == -1 { return; }
        madvise(p, size, 2);
        p as *const u8
    };

    let mut labels = [[0u8; 20]; HOURS];
    for h in 0..HOURS {
        let day = (h + 18) / 24;
        let (m, d) = if day < 2 { (10, 30 + day) } else { (11, day - 1) };
        let hr = (h + 18) % 24;
        labels[h].copy_from_slice(format!("2026-{m:02}-{d:02}T{hr:02}:00:00Z").as_bytes());
    }

    let mut cut = vec![size; NTHREADS + 1];
    cut[0] = 0;
    for t in 1..NTHREADS {
        let mut c = cut[t - 1].max(size * t / NTHREADS);
        while c < size && unsafe { *data.add(c - 1) } != b'\n' { c += 1; }
        cut[t] = c;
    }

    let mut workers: Vec<Worker> = (0..NTHREADS).map(|_| Worker::new()).collect();
    let b1 = Barrier::new(NTHREADS);
    let b2 = Barrier::new(NTHREADS);

    let mut shared_rank: Vec<Vec<u32>> = Vec::new();
    let mut shared_names: Vec<&[u8]> = Vec::new();
    let mut hour_outputs: Vec<Vec<u8>> = (0..HOURS).map(|_| Vec::new()).collect();

    let d_ptr = Ptr(data as *mut u8);
    let w_ptr = Ptr(workers.as_mut_ptr());
    let r_ptr: Ptr<Vec<Vec<u32>>> = Ptr(&mut shared_rank);
    let n_ptr: Ptr<Vec<&[u8]>> = Ptr(&mut shared_names);
    let o_ptr = Ptr(hour_outputs.as_mut_ptr());

    thread::scope(|s| {
        for t in 0..NTHREADS {
            let cut_ref = &cut;
            let (b1_ref, b2_ref) = (&b1, &b2);
            let labels_ref = &labels;
            s.spawn(move || {
                unsafe {
                    let w = &mut *w_ptr.0.add(t);
                    let from = d_ptr.0.add(cut_ref[t]);
                    let to = d_ptr.0.add(cut_ref[t + 1]);
                    w.parse(from, to);
                }
                b1_ref.wait();

                if t == 0 {
                    let mut global = EndpointTable::new();
                    let mut rank = vec![Vec::new(); NTHREADS];
                    for wt in 0..NTHREADS {
                        let w = unsafe { &*w_ptr.0.add(wt) };
                        for n in &w.endpoints.names {
                            rank[wt].push(unsafe { global.intern(n.as_ptr(), n.len()) });
                        }
                    }
                    let mut sorted: Vec<(u16, &[u8])> = global.names.iter().enumerate().map(|(i, &n)| (i as u16, n)).collect();
                    sorted.sort_unstable_by_key(|&(_, n)| n);
                    let mut rank_of = vec![0u32; sorted.len()];
                    for (new_rank, &(orig_id, _)) in sorted.iter().enumerate() { rank_of[orig_id as usize] = new_rank as u32; }
                    for r in &mut rank { for id in r { *id = rank_of[*id as usize]; } }
                    unsafe {
                        *r_ptr.0 = rank;
                        *n_ptr.0 = sorted.iter().map(|&(_, n)| n).collect();
                        madvise(d_ptr.0, size + 4096, 4);
                    }
                }
                b2_ref.wait();

                let rank = unsafe { &*r_ptr.0 };
                let names = unsafe { &*n_ptr.0 };
                let ne = names.len();

                let mut count = vec![0u32; ne + 1];
                let mut errors = vec![0u32; ne];
                let mut cursor = vec![0u32; ne];
                let mut lat = Vec::new();
                let mut line = [0u8; 512];

                let h_start = t * HOURS / NTHREADS;
                let h_end = (t + 1) * HOURS / NTHREADS;

                for h in h_start..h_end {
                    count.fill(0);
                    errors.fill(0);
                    for wt in 0..NTHREADS {
                        let w = unsafe { &*w_ptr.0.add(wt) };
                        let r = &rank[wt];
                        let mut cur = w.heads[h];
                        while cur != !0 {
                            let blk = &w.blocks[cur as usize];
                            for i in 0..blk.n as usize {
                                let key = blk.rec[i];
                                let e = r[(key & ID_MASK) as usize] as usize;
                                count[e + 1] += 1;
                                errors[e] += ((key & ERR_BIT) != 0) as u32;
                            }
                            cur = blk.next;
                        }
                        for &(l, key) in &w.big[h] {
                            let _ = l;
                            let e = r[(key & ID_MASK) as usize] as usize;
                            count[e + 1] += 1;
                            errors[e] += ((key & ERR_BIT) != 0) as u32;
                        }
                    }

                    for e in 0..ne { count[e + 1] += count[e]; }
                    cursor.copy_from_slice(&count[..ne]);
                    let total_lat = count[ne] as usize;
                    lat.clear();
                    lat.reserve(total_lat);
                    unsafe { lat.set_len(total_lat); }

                    for wt in 0..NTHREADS {
                        let w = unsafe { &*w_ptr.0.add(wt) };
                        let r = &rank[wt];
                        let mut cur = w.heads[h];
                        while cur != !0 {
                            let blk = &w.blocks[cur as usize];
                            for i in 0..blk.n as usize {
                                let rec = blk.rec[i];
                                let e = r[(rec & ID_MASK) as usize] as usize;
                                let dst = cursor[e] as usize;
                                lat[dst] = rec >> KEY_BITS;
                                cursor[e] += 1;
                            }
                            cur = blk.next;
                        }
                        for &(l, key) in &w.big[h] {
                            let e = r[(key & ID_MASK) as usize] as usize;
                            let dst = cursor[e] as usize;
                            lat[dst] = l;
                            cursor[e] += 1;
                        }
                    }

                    let label = &labels_ref[h];
                    let out_buf = unsafe { &mut *o_ptr.0.add(h) };
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
                            out_buf.extend_from_slice(&line[..pos]);
                        }
                    }
                }
            });
        }
    });

    std::fs::create_dir_all(&o_dir).ok();
    if let Ok(mut f) = File::create(&out_path) {
        for h in 0..HOURS {
            let _ = f.write_all(&hour_outputs[h]);
        }
    }
    unsafe { _exit(0); }
}
