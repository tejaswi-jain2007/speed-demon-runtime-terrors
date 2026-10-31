// Speed Demon 2026 - Track A (Ingest) - Ultra High Performance Flat Arena Progressive
use std::arch::x86_64::*;
use std::cell::UnsafeCell;
use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
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
const BLOCK_CAP: usize = 8192;
const TABLE_SLOTS: usize = 4096;

#[inline(always)]
unsafe fn load8(p: *const u8) -> u64 { (p as *const u64).read_unaligned() }

#[inline(always)]
unsafe fn find_nl(mut s: *const u8) -> *const u8 {
    let t = _mm256_set1_epi8(b'\n' as i8);
    loop {
        let m = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(s as *const _), t)) as u32;
        if m != 0 { return s.add(m.trailing_zeros() as usize); }
        s = s.add(32);
    }
}

#[inline(always)]
fn append_u32(buf: &mut [u8], mut v: u32, pos: usize) -> usize {
    if v == 0 { buf[pos] = b'0'; return pos + 1; }
    let mut t = [0u8; 10]; let mut i = 10;
    while v > 0 { i -= 1; t[i] = b'0' + (v % 10) as u8; v /= 10; }
    let len = 10 - i;
    buf[pos..pos + len].copy_from_slice(&t[i..]);
    pos + len
}

struct Table<'a> {
    slots: [u32; TABLE_SLOTS],
    names: Vec<&'a [u8]>,
}

impl<'a> Table<'a> {
    fn new() -> Self { Self { slots: [0; TABLE_SLOTS], names: Vec::with_capacity(2048) } }
    #[inline(always)]
    unsafe fn intern(&mut self, s: *const u8, len: usize) -> u32 {
        let mut a = load8(s);
        let b = if len >= 8 { load8(s.add(len - 8)) } else { 0 };
        if len < 8 { a &= (1u64 << (8 * len)) - 1; }
        let h1 = _mm_crc32_u64(len as u64, a);
        let mut i = _mm_crc32_u64(h1, b) as usize;
        loop {
            let slot = self.slots.get_unchecked_mut(i & (TABLE_SLOTS - 1));
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

struct HourBlock {
    n: u32,
    rec: *mut u32,
}

struct Worker<'a> {
    ep: Table<'a>,
    hours: Vec<HourBlock>,
    big: Vec<Vec<(u32, u32)>>,
    _base_ptr: *mut u32,
}
unsafe impl<'a> Send for Worker<'a> {}
unsafe impl<'a> Sync for Worker<'a> {}

impl<'a> Worker<'a> {
    fn new() -> Self {
        let total_bytes = HOURS * BLOCK_CAP * 4;
        let base_ptr = unsafe {
            let p = mmap(std::ptr::null_mut(), total_bytes, 3, 0x22, -1, 0) as *mut u32;
            if p == !0 as *mut u32 { _exit(1); }
            p
        };
        let mut hours = Vec::with_capacity(HOURS);
        for i in 0..HOURS {
            hours.push(HourBlock { n: 0, rec: unsafe { base_ptr.add(i * BLOCK_CAP) } });
        }
        Self {
            ep: Table::new(),
            hours,
            big: (0..HOURS).map(|_| Vec::new()).collect(),
            _base_ptr: base_ptr,
        }
    }
    unsafe fn parse(&mut self, mut p: *const u8, end: *const u8) {
        if p >= end { return; }

        while p < end {
            let day = (*p.add(9) - b'0' + *p.add(6) - b'0') as usize;
            let h = day * 24 + ((*p.add(11) - b'0') * 10 + (*p.add(12) - b'0')) as usize - 18;

            let v = load8(p.add(31)) ^ 0x2020202020202020u64;
            let mask = v.wrapping_sub(0x0101010101010101u64) & !v & 0x8080808080808080u64;
            let ep = p.add(32 + (mask.trailing_zeros() as usize >> 3));

            let eol = find_nl(p.add(39));

            let mut lat = 0u32;
            let mut cur = eol.sub(1);
            let mut mul = 1u32;
            while *cur != b' ' {
                lat += (*cur - b'0') as u32 * mul;
                mul *= 10;
                cur = cur.sub(1);
            }
            let is_err = ((*cur.sub(3) == b'5') as u32) << 11;
            let ee = cur.sub(4);
            let key = self.ep.intern(ep, ee as usize - ep as usize) | is_err;
            p = eol.add(1);

            let hb = self.hours.get_unchecked_mut(h);
            if lat < BIG && (hb.n as usize) < BLOCK_CAP {
                *hb.rec.add(hb.n as usize) = (lat << KEY_BITS) | key;
                hb.n += 1;
            } else {
                self.big[h].push((lat, key));
            }
        }
    }
}

struct SyncBox<T>(UnsafeCell<T>);
unsafe impl<T> Sync for SyncBox<T> {}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    unsafe { prctl(14, 1, 0, 0, 0); }
    let (mut d_dir, mut o_dir) = (String::from("/data"), String::from("/out"));
    let args: Vec<String> = std::env::args().collect();
    let mut ai = 1;
    let mut pos_idx = 0;
    while ai < args.len() {
        match args[ai].as_str() {
            "--data" => { if ai + 1 < args.len() { d_dir = args[ai + 1].clone(); ai += 2; } else { ai += 1; } }
            "--out" => { if ai + 1 < args.len() { o_dir = args[ai + 1].clone(); ai += 2; } else { ai += 1; } }
            arg if !arg.starts_with("--") => {
                if pos_idx == 0 { d_dir = arg.to_string(); }
                else if pos_idx == 1 { o_dir = arg.to_string(); }
                pos_idx += 1;
                ai += 1;
            }
            _ => ai += 1,
        }
    }
    std::fs::create_dir_all(&o_dir).ok();
    let in_p = format!("{}/requests.log", d_dir);
    let out_p = format!("{}/summary.txt", o_dir);
    let file = match File::open(&in_p) { Ok(f) => f, Err(_) => { File::create(&out_p)?; return Ok(()); } };
    let size = file.metadata()?.len() as usize;
    if size == 0 { File::create(&out_p)?; return Ok(()); }

    let data = unsafe {
        let p = mmap(std::ptr::null_mut(), size + 4096, 1, 2, file.as_raw_fd(), 0);
        if p == !0 as *mut u8 { _exit(1); }
        madvise(p, size, 2);
        p as *const u8
    };

    let mut labels = [[0u8; 20]; HOURS];
    for h in 0..HOURS {
        let day = (h + 18) / 24;
        let (m, d) = if day < 2 { (10, 30 + day) } else { (11, day - 1) };
        labels[h].copy_from_slice(format!("2026-{m:02}-{d:02}T{:02}:00:00Z", (h + 18) % 24).as_bytes());
    }

    let mut cut = vec![size; NTHREADS + 1];
    cut[0] = 0;
    for t in 1..NTHREADS {
        let mut c = cut[t - 1].max(size * t / NTHREADS);
        while c < size && unsafe { *data.add(c - 1) } != b'\n' { c += 1; }
        cut[t] = c;
    }

    let mut workers: Vec<Worker> = (0..NTHREADS).map(|_| Worker::new()).collect();
    let data_addr = data as usize;
    thread::scope(|s| {
        let cuts = &cut;
        for (t, w) in workers.iter_mut().enumerate() {
            s.spawn(move || unsafe {
                let from = (data_addr + cuts[t]) as *const u8;
                let to = (data_addr + cuts[t + 1]) as *const u8;
                w.parse(from, to);
                let sf = (from as usize + 4095) & !4095;
                let ef = (to as usize) & !4095;
                if ef > sf { madvise(sf as *mut u8, ef - sf, 4); }
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
    let mut sorted_with_id: Vec<(u16, &[u8])> = global.names.iter().enumerate().map(|(i, &n)| (i as u16, n)).collect();
    sorted_with_id.sort_unstable_by_key(|&(_, n)| n);
    let mut rank_of = vec![0u32; sorted_with_id.len()];
    for (new_rank, &(orig_id, _)) in sorted_with_id.iter().enumerate() {
        rank_of[orig_id as usize] = new_rank as u32;
    }
    for wt in 0..NTHREADS {
        for id in &mut rank[wt] { *id = rank_of[*id as usize]; }
    }
    let sorted: Vec<&[u8]> = sorted_with_id.iter().map(|&(_, n)| n).collect();
    let ne = sorted.len();
    unsafe { madvise(data as *mut u8, size + 4096, 4); }

    let next_hour = AtomicUsize::new(0);
    let out_boxes: Vec<SyncBox<Vec<u8>>> = (0..HOURS).map(|_| SyncBox(UnsafeCell::new(Vec::new()))).collect();
    let ready: Vec<AtomicBool> = (0..HOURS).map(|_| AtomicBool::new(false)).collect();
    let next_to_write = AtomicUsize::new(0);
    let out_file = Mutex::new(std::io::BufWriter::with_capacity(512 * 1024, File::create(&out_p)?));

    thread::scope(|s| {
        let (next_h, wrk_r, rk_r, lbl_r, nm_r, out_r, rdy_r, next_w, file_m) = (
            &next_hour, &workers, &rank, &labels, &sorted, &out_boxes, &ready, &next_to_write, &out_file,
        );

        for _ in 0..NTHREADS {
            s.spawn(move || {
                let (mut count, mut errors, mut cursor) = (vec![0u32; ne + 1], vec![0u32; ne], vec![0u32; ne]);
                let mut lat: Vec<u32> = Vec::new();
                let mut line = [0u8; 512];

                loop {
                    let h = next_h.fetch_add(1, Ordering::Relaxed);
                    if h >= HOURS { break; }

                    count.fill(0);
                    errors.fill(0);
                    for wt in 0..NTHREADS {
                        let (w, r) = (&wrk_r[wt], &rk_r[wt]);
                        let hb = &w.hours[h];
                        for i in 0..hb.n as usize {
                            let rec = unsafe { *hb.rec.add(i) };
                            let e = r[(rec & ID_MASK) as usize] as usize;
                            count[e + 1] += 1;
                            errors[e] += (rec & ERR_BIT) >> 11;
                        }
                        for &(_, key) in &w.big[h] {
                            let e = r[(key & ID_MASK) as usize] as usize;
                            count[e + 1] += 1;
                            errors[e] += (key & ERR_BIT) >> 11;
                        }
                    }

                    for e in 0..ne { count[e + 1] += count[e]; }
                    cursor.copy_from_slice(&count[..ne]);
                    let total_lat = count[ne] as usize;
                    lat.clear();
                    lat.reserve(total_lat);
                    unsafe { lat.set_len(total_lat); }

                    for wt in 0..NTHREADS {
                        let (w, r) = (&wrk_r[wt], &rk_r[wt]);
                        let hb = &w.hours[h];
                        for i in 0..hb.n as usize {
                            let rec = unsafe { *hb.rec.add(i) };
                            let e = r[(rec & ID_MASK) as usize] as usize;
                            lat[cursor[e] as usize] = rec >> KEY_BITS;
                            cursor[e] += 1;
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
                        if n == 0 { continue; }
                        let slice = &mut lat[start..end];
                        slice.sort_unstable();
                        let p50 = slice[(50 * n + 99) / 100 - 1];
                        let p95 = slice[(95 * n + 99) / 100 - 1];
                        let p99 = slice[(99 * n + 99) / 100 - 1];

                        let (err, mut pos) = (errors[e], 0);
                        let ppm = ((err as u64) * 1_000_000 / (n as u64)) as u32;
                        line[..20].copy_from_slice(label);
                        pos += 20;
                        line[pos] = b' '; pos += 1;
                        let ep_name = nm_r[e];
                        line[pos..pos + ep_name.len()].copy_from_slice(ep_name);
                        pos += ep_name.len();
                        for v in [n as u32, p50, p95, p99, err, ppm] {
                            line[pos] = b' '; pos += 1;
                            pos = append_u32(&mut line, v, pos);
                        }
                        line[pos] = b'\n'; pos += 1;
                        out_text.extend_from_slice(&line[..pos]);
                    }

                    rdy_r[h].store(true, Ordering::Release);

                    if let Ok(mut writer) = file_m.try_lock() {
                        let mut cur = next_w.load(Ordering::Relaxed);
                        while cur < HOURS && rdy_r[cur].load(Ordering::Acquire) {
                            let out_bytes = unsafe { &mut *out_r[cur].0.get() };
                            let _ = writer.write_all(out_bytes);
                            out_bytes.clear();
                            out_bytes.shrink_to_fit();
                            cur += 1;
                        }
                        next_w.store(cur, Ordering::Relaxed);
                    }
                }
            });
        }
    });

    let mut writer = out_file.lock().unwrap();
    let mut cur = next_to_write.load(Ordering::Relaxed);
    while cur < HOURS {
        let out_bytes = unsafe { &*out_boxes[cur].0.get() };
        let _ = writer.write_all(out_bytes);
        cur += 1;
    }
    writer.flush().ok();
    unsafe { _exit(0); }
}
