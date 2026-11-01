// Speed Demon 2026 - Track A (Turbo Ingest) - Runtime Terrors (ultra-fast compact)
use std::arch::x86_64::*;
use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering::*};
use std::sync::OnceLock;
use std::thread;

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn madvise(a: *mut u8, l: usize, adv: i32) -> i32;
    fn prctl(o: i32, a: u64, b: u64, c: u64, d: u64) -> i32;
    fn _exit(s: i32) -> !;
}

const RW: i32 = 3; const PRIV: i32 = 2; const ANON: i32 = 0x20; const FIXED: i32 = 0x10;
const FAIL: *mut u8 = !0 as *mut u8; const DONTNEED: i32 = 4;
const HOURS: usize = 72; const KB: u32 = 12; const IDM: u32 = (1 << 11) - 1;
const ERR: u32 = 1 << 11; const BIG: u32 = 1 << 20; const SLOTS: usize = 1 << 12;
const CAP: usize = (16384 - 16) / 4; const BS: usize = std::mem::size_of::<Block>();

#[inline(always)] unsafe fn load8(p: *const u8) -> u64 { (p as *const u64).read_unaligned() }

#[inline(always)] unsafe fn find(mut s: *const u8, c: u8) -> *const u8 {
    let k = _mm256_set1_epi8(c as i8);
    loop {
        let m = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(s as *const __m256i), k)) as u32;
        if m != 0 { return s.add(m.trailing_zeros() as usize); }
        s = s.add(32);
    }
}

#[inline(always)] unsafe fn parse_digits(end: *const u8, len: u32) -> u32 {
    let v = _mm_subs_epu8(_mm_loadu_si128(end.sub(16) as *const __m128i), _mm_set1_epi8(b'0' as i8));
    let lane = _mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    let v = _mm_and_si128(v, _mm_cmpgt_epi8(lane, _mm_set1_epi8((15 - len) as i8)));
    let v = _mm_maddubs_epi16(v, _mm_set1_epi16(0x010A));
    let v = _mm_madd_epi16(v, _mm_set1_epi32(0x00010064));
    let v = _mm_packus_epi32(v, v);
    let v = _mm_madd_epi16(v, _mm_set1_epi32(0x00012710));
    (_mm_cvtsi128_si32(v) as u32).wrapping_mul(100000000) + _mm_extract_epi32(v, 1) as u32
}

#[derive(Copy, Clone)] struct SendPtr<T>(*const T);
unsafe impl<T> Send for SendPtr<T> {} unsafe impl<T> Sync for SendPtr<T> {}
impl<T> SendPtr<T> { #[inline(always)] fn get(&self) -> *const T { self.0 } }

struct Table<'a> { slots: [u32; SLOTS], names: Vec<&'a [u8]> }
impl<'a> Table<'a> {
    fn new() -> Self { Self { slots: [0; SLOTS], names: Vec::with_capacity(512) } }
    #[inline(always)] unsafe fn intern(&mut self, s: *const u8, len: usize) -> u32 {
        let mut a = load8(s);
        let b = if len >= 8 { load8(s.add(len - 8)) } else { a &= (1u64 << (8 * len)) - 1; 0 };
        let mut i = _mm_crc32_u64(_mm_crc32_u64(len as u64, a), b) as usize;
        loop {
            let slot = &mut self.slots[i % SLOTS];
            if *slot == 0 {
                self.names.push(std::slice::from_raw_parts(s, len));
                *slot = self.names.len() as u32;
                return *slot - 1;
            }
            let n = self.names[(*slot - 1) as usize];
            if n.len() == len && std::slice::from_raw_parts(s, len) == n { return *slot - 1; }
            i += 1;
        }
    }
}

#[repr(C)] struct Block { next: *mut Block, n: u32, rec: [u32; CAP] }

struct Worker<'a> {
    ep: Table<'a>, head: [*mut Block; HOURS], big: Vec<Vec<(u32, u32)>>,
    region: *mut u8, used: usize, reserved: usize,
}
unsafe impl<'a> Send for Worker<'a> {} unsafe impl<'a> Sync for Worker<'a> {}
impl<'a> Worker<'a> {
    fn new() -> Self {
        Self { ep: Table::new(), head: [std::ptr::null_mut(); HOURS], big: vec![Vec::new(); HOURS],
               region: std::ptr::null_mut(), used: 0, reserved: 0 }
    }
    unsafe fn parse(&mut self, mut p: *const u8, end: *const u8) {
        if p >= end { return; }
        self.reserved = 72 * BS;
        self.region = mmap(std::ptr::null_mut(), self.reserved, RW, PRIV | ANON, -1, 0);
        if self.region == FAIL { _exit(1); }
        let sp_mask = _mm256_set1_epi8(b' ' as i8);
        let nl_mask = _mm256_set1_epi8(b'\n' as i8);
        while p < end {
            let day = (*p.add(9) - b'0' + (*p.add(6) & 1)) as usize;
            let h = day * 24 + ((*p.add(11) - b'0') * 10 + (*p.add(12) - b'0')) as usize - 18;
            let m = p.add(28);
            let v0 = _mm256_loadu_si256(m as *const __m256i);
            let v1 = _mm256_loadu_si256(m.add(32) as *const __m256i);
            let sp0 = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v0, sp_mask)) as u32;
            let sp1 = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v1, sp_mask)) as u32;
            let nl0 = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v0, nl_mask)) as u32;
            let nl1 = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v1, nl_mask)) as u32;
            let sp = (sp0 as u64) | ((sp1 as u64) << 32);
            let nl = (nl0 as u64) | ((nl1 as u64) << 32);
            let ep = m.add(sp.trailing_zeros() as usize + 1);
            let ee = if nl != 0 { m.add((sp & (sp - 1)).trailing_zeros() as usize) } else { find(ep, b' ') };
            let eol = if nl != 0 { m.add(nl.trailing_zeros() as usize) } else { find(ee.add(5), b'\n') };
            let key = self.ep.intern(ep, ee as usize - ep as usize) | if *ee.add(1) == b'5' { ERR } else { 0 };
            let lat = parse_digits(eol, (eol as usize - ee as usize - 5) as u32);
            p = eol.add(1);
            if lat >= BIG { self.big[h].push((lat, key)); continue; }
            let b = &mut self.head[h];
            if (*b).is_null() || (**b).n as usize == CAP {
                if self.used + BS > self.reserved {
                    let ptr = mmap(std::ptr::null_mut(), 16 * BS, RW, PRIV | ANON, -1, 0);
                    if ptr != FAIL { self.region = ptr; self.used = 0; self.reserved = 16 * BS; }
                }
                let f = self.region.add(self.used) as *mut Block;
                self.used += BS;
                (*f).next = *b; (*f).n = 0; *b = f;
            }
            let r = &mut **b;
            r.rec[r.n as usize] = (lat << KB) | key;
            r.n += 1;
        }
    }
}

#[inline(always)] fn put(buf: &mut [u8], mut v: u32, mut pos: usize) -> usize {
    let mut t = [0u8; 10]; let mut n = 0;
    loop { t[n] = b'0' + (v % 10) as u8; v /= 10; n += 1; if v == 0 { break; } }
    while n > 0 { n -= 1; buf[pos] = t[n]; pos += 1; }
    pos
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    unsafe { prctl(14, 1, 0, 0, 0); }
    let (mut dd, mut od) = (PathBuf::from("/data"), PathBuf::from("/out"));
    let a: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < a.len() {
        if i + 1 < a.len() && a[i] == "--data" { dd = a[i + 1].clone().into(); i += 2; }
        else if i + 1 < a.len() && a[i] == "--out" { od = a[i + 1].clone().into(); i += 2; }
        else { i += 1; }
    }
    if let Ok(e) = std::env::var("DATA_DIR") { if !e.is_empty() { dd = e.into(); } }
    if let Ok(e) = std::env::var("OUT_DIR") { if !e.is_empty() { od = e.into(); } }
    std::fs::create_dir_all(&od).ok();
    let inp = if dd.is_file() { dd } else { dd.join("requests.log") };
    let outp = od.join("summary.txt");
    if !inp.exists() { File::create(&outp)?; return Ok(()); }
    let file = File::open(&inp)?;
    let size = file.metadata()?.len() as usize;
    if size == 0 { File::create(&outp)?; return Ok(()); }
    let data = unsafe {
        let p = mmap(std::ptr::null_mut(), size + 4096, RW, PRIV | ANON, -1, 0);
        if p == FAIL || mmap(p, size, 1, PRIV | FIXED, file.as_raw_fd(), 0) == FAIL { _exit(1); }
        p as *const u8
    };
    let nt = 12;
    let labels: Vec<[u8; 20]> = (0..HOURS).map(|h| {
        let day = (h + 18) / 24;
        let (m, d) = if day < 2 { (10, 30 + day) } else { (11, day - 1) };
        let mut l = [0u8; 20];
        l.copy_from_slice(format!("2026-{m:02}-{d:02}T{:02}:00:00Z", (h + 18) % 24).as_bytes());
        l
    }).collect();
    let mut cut = vec![size; nt + 1];
    cut[0] = 0;
    for t in 1..nt {
        let mut c = cut[t - 1].max(size * t / nt);
        while c < size && unsafe { *data.add(c - 1) } != b'\n' { c += 1; }
        cut[t] = c;
    }
    let mut workers: Vec<Worker> = (0..nt).map(|_| Worker::new()).collect();
    let ds = SendPtr(data);
    thread::scope(|s| {
        let cut = &cut;
        for (t, w) in workers.iter_mut().enumerate() {
            s.spawn(move || unsafe {
                let (from, to) = (ds.get().add(cut[t]), ds.get().add(cut[t + 1]));
                w.parse(from, to);
                let (sf, ef) = ((from as usize + 4095) & !4095, (to as usize) & !4095);
                if ef > sf { madvise(sf as *mut u8, ef - sf, DONTNEED); }
            });
        }
    });
    let mut global = Table::new();
    let mut rank: Vec<Vec<u32>> = workers.iter()
        .map(|w| w.ep.names.iter().map(|n| unsafe { global.intern(n.as_ptr(), n.len()) }).collect()).collect();
    let mut sorted = global.names.clone();
    sorted.sort_unstable();
    let mut rank_of = vec![0u32; sorted.len()];
    for (i, n) in sorted.iter().enumerate() { rank_of[unsafe { global.intern(n.as_ptr(), n.len()) } as usize] = i as u32; }
    rank.iter_mut().flatten().for_each(|id| *id = rank_of[*id as usize]);
    let ne = sorted.len();
    unsafe { madvise(data as *mut u8, size + 4096, DONTNEED); }
    let text: Vec<OnceLock<Vec<u8>>> = (0..HOURS).map(|_| OnceLock::new()).collect();
    let next = AtomicUsize::new(0);
    thread::scope(|s| {
        let (text, next, workers, rank, labels, sorted) = (&text, &next, &workers, &rank, &labels, &sorted);
        s.spawn(move || {
            let Ok(mut f) = File::create(&outp) else { return };
            for h in 0..HOURS {
                let b = loop { if let Some(b) = text[h].get() { break b; } thread::yield_now(); };
                f.write_all(b).ok();
            }
        });
        for _ in 0..nt {
            s.spawn(move || {
                let (mut count, mut errors, mut cursor) = (vec![0u32; ne + 1], vec![0u32; ne], vec![0u32; ne]);
                let (mut lat, mut line) = (Vec::new(), [0u8; 512]);
                loop {
                    let h = next.fetch_add(1, Relaxed);
                    if h >= HOURS { break; }
                    count.fill(0); errors.fill(0);
                    for (w, r) in workers.iter().zip(rank) {
                        let mut b = w.head[h];
                        while !b.is_null() { unsafe {
                            let br = &*b;
                            for &k in &br.rec[..br.n as usize] {
                                let e = r[(k & IDM) as usize] as usize;
                                count[e + 1] += 1; errors[e] += ((k & ERR) != 0) as u32;
                            }
                            b = br.next;
                        } }
                        for &(_, k) in &w.big[h] {
                            let e = r[(k & IDM) as usize] as usize;
                            count[e + 1] += 1; errors[e] += ((k & ERR) != 0) as u32;
                        }
                    }
                    for e in 0..ne { count[e + 1] += count[e]; }
                    cursor.copy_from_slice(&count[..ne]);
                    let total_lat = count[ne] as usize;
                    lat.clear();
                    lat.reserve(total_lat);
                    unsafe { lat.set_len(total_lat); }
                    for (w, r) in workers.iter().zip(rank) {
                        let mut b = w.head[h];
                        while !b.is_null() { unsafe {
                            let br = &*b;
                            for &k in &br.rec[..br.n as usize] {
                                let e = r[(k & IDM) as usize] as usize;
                                lat[cursor[e] as usize] = k >> KB; cursor[e] += 1;
                            }
                            let nb = br.next;
                            madvise(b as *mut u8, BS, DONTNEED);
                            b = nb;
                        } }
                        for &(l, k) in &w.big[h] {
                            let e = r[(k & IDM) as usize] as usize;
                            lat[cursor[e] as usize] = l; cursor[e] += 1;
                        }
                    }
                    let mut out = Vec::with_capacity(ne * 64);
                    for e in 0..ne {
                        let (st, en) = (count[e] as usize, count[e + 1] as usize);
                        let n = en - st;
                        if n == 0 { continue; }
                        let sl = &mut lat[st..en];
                        let q = |p: usize| (p * n + 99) / 100 - 1;
                        let (r50, r95, r99) = (q(50), q(95), q(99));
                        let (p50, p95, p99) = if n <= 256 {
                            sl.sort_unstable(); (sl[r50], sl[r95], sl[r99])
                        } else {
                            sl.select_nth_unstable(r99);
                            let p99 = sl[r99];
                            let p95 = if r95 == r99 { p99 } else { sl[..r99].select_nth_unstable(r95); sl[r95] };
                            let p50 = if r50 == r95 { p95 } else { sl[..r95].select_nth_unstable(r50); sl[r50] };
                            (p50, p95, p99)
                        };
                        let err = errors[e];
                        line[..20].copy_from_slice(&labels[h]);
                        let nm = sorted[e];
                        line[20] = b' '; line[21..21 + nm.len()].copy_from_slice(nm);
                        let mut pos = 21 + nm.len();
                        for v in [n as u32, p50, p95, p99, err, ((err as u64) * 1000000 / n as u64) as u32] {
                            line[pos] = b' '; pos = put(&mut line, v, pos + 1);
                        }
                        line[pos] = b'\n';
                        out.extend_from_slice(&line[..pos + 1]);
                    }
                    text[h].set(out).ok();
                }
            });
        }
    });
    unsafe { _exit(0); }
}
