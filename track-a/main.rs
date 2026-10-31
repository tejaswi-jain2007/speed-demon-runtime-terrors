// Speed Demon 2026 - Track A (Ingest) - Runtime Terrors
use std::arch::x86_64::*;
use std::cell::UnsafeCell;
use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

extern "C" {
    fn mmap(a: *mut u8, l: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn madvise(a: *mut u8, l: usize, adv: i32) -> i32;
    fn prctl(opt: i32, a2: u64, a3: u64, a4: u64, a5: u64) -> i32;
    fn _exit(status: i32) -> !;
}

const HOURS: usize = 72;
const KEY_BITS: u32 = 12;
const ID_MASK: u32 = (1 << 11) - 1;
const ERR_BIT: u32 = 1 << 11;
const BIG: u32 = 1 << 20;
const NTHREADS: usize = 12;
const BLOCK_CAP: usize = (16384 - 16) / 4;
const SLOTS: usize = 8192;

#[inline(always)]
unsafe fn load8(p: *const u8) -> u64 {
    (p as *const u64).read_unaligned()
}

#[inline(always)]
unsafe fn find_nl(mut s: *const u8) -> *const u8 {
    let t = _mm256_set1_epi8(b'\n' as i8);
    loop {
        let m =
            _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(s as *const _), t)) as u32;
        if m != 0 {
            return s.add(m.trailing_zeros() as usize);
        }
        s = s.add(32);
    }
}

#[inline(always)]
unsafe fn parse_digits(end: *const u8, len: u32) -> u32 {
    let v = _mm_subs_epu8(
        _mm_loadu_si128(end.sub(16) as *const __m128i),
        _mm_set1_epi8(b'0' as i8),
    );
    let lane = _mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    let v = _mm_and_si128(v, _mm_cmpgt_epi8(lane, _mm_set1_epi8((15 - len) as i8)));
    let v = _mm_maddubs_epi16(v, _mm_set1_epi16(0x010A));
    let v = _mm_madd_epi16(v, _mm_set1_epi32(0x00010064));
    let v = _mm_packus_epi32(v, v);
    let v = _mm_madd_epi16(v, _mm_set1_epi32(0x00012710));
    (_mm_cvtsi128_si32(v) as u32).wrapping_mul(100000000) + (_mm_extract_epi32(v, 1) as u32)
}

#[inline(always)]
fn append_u32(buf: &mut [u8], mut val: u32, mut pos: usize) -> usize {
    if val == 0 {
        buf[pos] = b'0';
        return pos + 1;
    }
    let (mut temp, mut len) = ([0u8; 10], 0);
    while val > 0 {
        temp[len] = b'0' + (val % 10) as u8;
        val /= 10;
        len += 1;
    }
    while len > 0 {
        len -= 1;
        buf[pos] = temp[len];
        pos += 1;
    }
    pos
}

#[repr(C)]
#[derive(Copy, Clone)]
struct Slot {
    fp: u64,
    id: u16,
    len: u16,
    ptr: *const u8,
}

struct Table<'a> {
    slots: [Slot; SLOTS],
    names: Vec<&'a [u8]>,
}

impl<'a> Table<'a> {
    fn new() -> Self {
        Self {
            slots: [Slot {
                fp: 0,
                id: 0,
                len: 0,
                ptr: std::ptr::null(),
            }; SLOTS],
            names: Vec::with_capacity(256),
        }
    }
    #[inline(always)]
    unsafe fn intern(&mut self, s: *const u8, len: usize) -> u32 {
        let mut a = load8(s);
        let b = if len >= 8 { load8(s.add(len - 8)) } else { 0 };
        let c = if len >= 16 {
            load8(s.add((len - 8) / 2))
        } else {
            0
        };
        if len < 8 {
            a &= (1u64 << (8 * len)) - 1;
        }
        let h = _mm_crc32_u64(_mm_crc32_u64(_mm_crc32_u64(len as u64, a), c), b);
        let fp = h | 1;
        let mut i = (h as usize) & (SLOTS - 1);
        loop {
            let slot = self.slots.get_unchecked_mut(i);
            if slot.fp == 0 {
                let idx = self.names.len() as u16;
                *slot = Slot {
                    fp,
                    id: idx,
                    len: len as u16,
                    ptr: s,
                };
                self.names.push(std::slice::from_raw_parts(s, len));
                return idx as u32;
            }
            if slot.fp == fp
                && slot.len as usize == len
                && std::slice::from_raw_parts(slot.ptr, len) == std::slice::from_raw_parts(s, len)
            {
                return slot.id as u32;
            }
            i = (i + 1) & (SLOTS - 1);
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
    ep: Table<'a>,
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
            ep: Table::new(),
            head: [std::ptr::null_mut(); HOURS],
            big: (0..HOURS).map(|_| Vec::new()).collect(),
            region: std::ptr::null_mut(),
            used: 0,
            reserved: 0,
        }
    }
    unsafe fn parse(&mut self, mut p: *const u8, end: *const u8) {
        if p >= end {
            return;
        }
        self.reserved = 32 * std::mem::size_of::<Block>();
        self.region = mmap(std::ptr::null_mut(), self.reserved, 3, 0x22, -1, 0);
        if self.region == !0 as *mut u8 {
            _exit(1);
        }

        let sp_vec128 = _mm_set1_epi8(b' ' as i8);
        let mut cur_b: [*mut Block; HOURS] = [std::ptr::null_mut(); HOURS];

        while p < end {
            let day = (*p.add(9) - b'0' + *p.add(6) - b'0') as usize;
            let h = day * 24 + ((*p.add(11) - b'0') * 10 + (*p.add(12) - b'0')) as usize - 18;

            let v = load8(p.add(31)) ^ 0x2020202020202020u64;
            let mask = v.wrapping_sub(0x0101010101010101u64) & !v & 0x8080808080808080u64;
            let ep = p.add(32 + (mask.trailing_zeros() as usize >> 3));

            let eol = find_nl(p.add(39));

            let chunk = _mm_loadu_si128(eol.sub(16) as *const __m128i);
            let sp_mask = _mm_movemask_epi8(_mm_cmpeq_epi8(chunk, sp_vec128)) as u32;
            let sp_offset = 31 - sp_mask.leading_zeros() as usize;
            let sp_lat = eol.sub(16).add(sp_offset);

            let ee = sp_lat.sub(4);
            let is_err = ((*sp_lat.sub(3) == b'5') as u32) << 11;
            let key = self.ep.intern(ep, ee as usize - ep as usize) | is_err;
            let lat_len = eol as usize - sp_lat as usize - 1;
            let lat = parse_digits(eol, lat_len as u32);
            p = eol.add(1);

            if lat >= BIG {
                self.big[h].push((lat, key));
                continue;
            }
            let mut b = cur_b[h];
            if b.is_null() || (*b).n as usize == BLOCK_CAP {
                if self.used + std::mem::size_of::<Block>() > self.reserved {
                    let extra = 16 * std::mem::size_of::<Block>();
                    let p = mmap(std::ptr::null_mut(), extra, 3, 0x22, -1, 0);
                    if p != !0 as *mut u8 {
                        self.region = p;
                        self.used = 0;
                        self.reserved = extra;
                    }
                }
                let fresh = self.region.add(self.used) as *mut Block;
                self.used += std::mem::size_of::<Block>();
                (*fresh).next = self.head[h];
                self.head[h] = fresh;
                cur_b[h] = fresh;
                b = fresh;
            }
            let br = &mut *b;
            br.rec[br.n as usize] = (lat << KEY_BITS) | key;
            br.n += 1;
        }
    }
}

struct SyncBox<T>(UnsafeCell<T>);
unsafe impl<T> Sync for SyncBox<T> {}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        prctl(14, 1, 0, 0, 0);
    }
    let (mut d_dir, mut o_dir) = (PathBuf::from("/data"), PathBuf::from("/out"));
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--data" {
            if let Some(v) = args.next() {
                d_dir = PathBuf::from(v);
            }
        } else if arg == "--out" {
            if let Some(v) = args.next() {
                o_dir = PathBuf::from(v);
            }
        }
    }
    if let Ok(e) = std::env::var("DATA_DIR") {
        if !e.is_empty() {
            d_dir = PathBuf::from(e);
        }
    }
    if let Ok(e) = std::env::var("OUT_DIR") {
        if !e.is_empty() {
            o_dir = PathBuf::from(e);
        }
    }
    std::fs::create_dir_all(&o_dir).ok();

    let in_p = if d_dir.is_file() {
        d_dir
    } else {
        d_dir.join("requests.log")
    };
    let out_p = o_dir.join("summary.txt");
    if !in_p.exists() {
        File::create(&out_p)?;
        return Ok(());
    }
    let file = File::open(&in_p)?;
    let size = file.metadata()?.len() as usize;
    if size == 0 {
        File::create(&out_p)?;
        return Ok(());
    }

    let data = unsafe {
        let p = mmap(std::ptr::null_mut(), size + 4096, 1, 2, file.as_raw_fd(), 0);
        if p == !0 as *mut u8 {
            _exit(1);
        }
        madvise(p, size, 2);
        p as *const u8
    };

    let mut labels = [[0u8; 20]; HOURS];
    for h in 0..HOURS {
        let day = (h + 18) / 24;
        let (m, d) = if day < 2 {
            (10, 30 + day)
        } else {
            (11, day - 1)
        };
        labels[h]
            .copy_from_slice(format!("2026-{m:02}-{d:02}T{:02}:00:00Z", (h + 18) % 24).as_bytes());
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
    let data_addr = data as usize;

    thread::scope(|s| {
        let cuts = &cut;
        for (t, w) in workers.iter_mut().enumerate() {
            s.spawn(move || unsafe {
                w.parse(
                    (data_addr + cuts[t]) as *const u8,
                    (data_addr + cuts[t + 1]) as *const u8,
                );
            });
        }
    });

    let mut global = Table::new();
    let mut rank = vec![Vec::new(); NTHREADS];
    for wt in 0..NTHREADS {
        for n in &workers[wt].ep.names {
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
    for wt in 0..NTHREADS {
        for id in &mut rank[wt] {
            *id = rank_of[*id as usize];
        }
    }
    let sorted: Vec<&[u8]> = sorted_with_id.iter().map(|&(_, n)| n).collect();
    let ne = sorted.len();

    let next_hour = AtomicUsize::new(0);
    let out_boxes: Vec<SyncBox<Vec<u8>>> = (0..HOURS)
        .map(|_| SyncBox(UnsafeCell::new(Vec::new())))
        .collect();

    thread::scope(|s| {
        let (next_h, wrk_r, rk_r, lbl_r, nm_r, out_r) =
            (&next_hour, &workers, &rank, &labels, &sorted, &out_boxes);

        for _ in 0..NTHREADS {
            s.spawn(move || {
                let (mut count, mut errors, mut cursor) =
                    (vec![0u32; ne + 1], vec![0u32; ne], vec![0u32; ne]);
                let mut lat: Vec<u32> = Vec::new();
                let mut line = [0u8; 512];

                loop {
                    let h = next_h.fetch_add(1, Ordering::Relaxed);
                    if h >= HOURS {
                        break;
                    }

                    count.fill(0);
                    errors.fill(0);
                    for wt in 0..NTHREADS {
                        let (w, r) = (&wrk_r[wt], &rk_r[wt]);
                        let mut b = w.head[h];
                        while !b.is_null() {
                            unsafe {
                                let br = &*b;
                                for i in 0..br.n as usize {
                                    let rec = br.rec[i];
                                    let e = r[(rec & ID_MASK) as usize] as usize;
                                    count[e + 1] += 1;
                                    errors[e] += (rec & ERR_BIT) >> 11;
                                }
                                b = br.next;
                            }
                        }
                        for &(_, key) in &w.big[h] {
                            let e = r[(key & ID_MASK) as usize] as usize;
                            count[e + 1] += 1;
                            errors[e] += (key & ERR_BIT) >> 11;
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
                        let (w, r) = (&wrk_r[wt], &rk_r[wt]);
                        let mut b = w.head[h];
                        while !b.is_null() {
                            unsafe {
                                let br = &*b;
                                for i in 0..br.n as usize {
                                    let rec = br.rec[i];
                                    let e = r[(rec & ID_MASK) as usize] as usize;
                                    lat[cursor[e] as usize] = rec >> KEY_BITS;
                                    cursor[e] += 1;
                                }
                                b = br.next;
                            }
                        }
                        for &(l, key) in &w.big[h] {
                            let e = r[(key & ID_MASK) as usize] as usize;
                            lat[cursor[e] as usize] = l;
                            cursor[e] += 1;
                        }
                    }

                    let label = &lbl_r[h];
                    let out_text = unsafe { &mut *out_r[h].0.get() };
                    out_text.reserve(ne * 64);

                    for e in 0..ne {
                        let (start, end) = (count[e] as usize, count[e + 1] as usize);
                        let n = end - start;
                        if n == 0 {
                            continue;
                        }
                        let slice = &mut lat[start..end];
                        let (p50, p95, p99) = if n <= 2 {
                            if n == 1 {
                                (slice[0], slice[0], slice[0])
                            } else {
                                (
                                    slice[0].min(slice[1]),
                                    slice[0].max(slice[1]),
                                    slice[0].max(slice[1]),
                                )
                            }
                        } else if n <= 256 {
                            slice.sort_unstable();
                            (
                                slice[(50 * n + 99) / 100 - 1],
                                slice[(95 * n + 99) / 100 - 1],
                                slice[(99 * n + 99) / 100 - 1],
                            )
                        } else {
                            let (r99, r95, r50) = (
                                (99 * n + 99) / 100 - 1,
                                (95 * n + 99) / 100 - 1,
                                (50 * n + 99) / 100 - 1,
                            );
                            slice.select_nth_unstable(r99);
                            let p99 = slice[r99];
                            let p95 = if r95 == r99 {
                                p99
                            } else {
                                slice[..r99].select_nth_unstable(r95);
                                slice[r95]
                            };
                            let p50 = if r50 == r95 {
                                p95
                            } else {
                                slice[..r95].select_nth_unstable(r50);
                                slice[r50]
                            };
                            (p50, p95, p99)
                        };

                        let (err, mut pos) = (errors[e], 0);
                        let ppm = ((err as u64) * 1_000_000 / (n as u64)) as u32;
                        line[..20].copy_from_slice(label);
                        pos += 20;
                        line[pos] = b' ';
                        pos += 1;
                        let ep_name = nm_r[e];
                        line[pos..pos + ep_name.len()].copy_from_slice(ep_name);
                        pos += ep_name.len();
                        for v in [n as u32, p50, p95, p99, err, ppm] {
                            line[pos] = b' ';
                            pos += 1;
                            pos = append_u32(&mut line, v, pos);
                        }
                        line[pos] = b'\n';
                        pos += 1;
                        out_text.extend_from_slice(&line[..pos]);
                    }
                }
            });
        }
    });

    let mut writer = std::io::BufWriter::with_capacity(512 * 1024, File::create(&out_p)?);
    for h in 0..HOURS {
        writer.write_all(unsafe { &*out_boxes[h].0.get() }).ok();
    }
    writer.flush().ok();
    unsafe {
        _exit(0);
    }
}
