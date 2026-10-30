// Speed Demon 2026 - Track C (Join)
// Team: Runtime Terrors
// High-performance zero-dependency CSV join and streaming aggregation engine in Rust

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;

const PROT_READ: i32 = 1;
const MAP_PRIVATE: i32 = 2;
const MADV_WILLNEED: i32 = 3;

extern "C" {
    fn mmap(addr: *mut u8, length: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn munmap(addr: *mut u8, length: usize) -> i32;
    fn madvise(addr: *mut u8, length: usize, advice: i32) -> i32;
}

struct FastMmap {
    ptr: *const u8,
    len: usize,
    _file: File,
}

impl FastMmap {
    fn open(path: &str) -> Option<Self> {
        let file = File::open(path).ok()?;
        let len = file.metadata().ok()?.len() as usize;
        if len == 0 {
            return None;
        }

        let fd = file.as_raw_fd();
        let ptr = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, fd, 0) };
        if ptr as isize == -1 {
            return None;
        }

        unsafe { madvise(ptr, len, MADV_WILLNEED) };
        Some(FastMmap {
            ptr,
            len,
            _file: file,
        })
    }

    #[inline(always)]
    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for FastMmap {
    fn drop(&mut self) {
        unsafe {
            munmap(self.ptr as *mut u8, self.len);
        }
    }
}

#[inline(always)]
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3u64);
    }
    h
}

#[inline(always)]
fn parse_cents(bytes: &[u8]) -> i64 {
    if bytes.is_empty() {
        return 0;
    }
    let mut p = 0;
    let negative = if bytes[0] == b'-' {
        p += 1;
        true
    } else {
        false
    };

    let mut whole: i64 = 0;
    while p < bytes.len() && bytes[p] >= b'0' && bytes[p] <= b'9' {
        whole = whole * 10 + (bytes[p] - b'0') as i64;
        p += 1;
    }
    if p < bytes.len() && bytes[p] == b'.' {
        p += 1;
    }
    let mut frac: i64 = 0;
    if p < bytes.len() && bytes[p] >= b'0' && bytes[p] <= b'9' {
        frac = frac * 10 + (bytes[p] - b'0') as i64;
        p += 1;
    }
    if p < bytes.len() && bytes[p] >= b'0' && bytes[p] <= b'9' {
        frac = frac * 10 + (bytes[p] - b'0') as i64;
    }
    let cents = whole * 100 + frac;
    if negative { -cents } else { cents }
}

fn format_cents(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let c = cents.abs();
    format!("{}{}.{:02}", sign, c / 100, c % 100)
}

struct CountryAgg {
    count: u64,
    id_sum: u64,
    amount_sum: i64,
    amount_min: i64,
    amount_max: i64,
    name_bytes: u64,
    note_bytes: u64,
    name_hashes: Vec<u64>,
}

impl CountryAgg {
    fn new() -> Self {
        CountryAgg {
            count: 0,
            id_sum: 0,
            amount_sum: 0,
            amount_min: i64::MAX,
            amount_max: i64::MIN,
            name_bytes: 0,
            note_bytes: 0,
            name_hashes: Vec::with_capacity(4096),
        }
    }

    fn add(&mut self, id: u64, name_hash: u64, n_bytes: usize, nt_bytes: usize, cents: i64) {
        self.count += 1;
        self.id_sum += id;
        self.amount_sum += cents;
        if cents < self.amount_min {
            self.amount_min = cents;
        }
        if cents > self.amount_max {
            self.amount_max = cents;
        }
        self.name_bytes += n_bytes as u64;
        self.note_bytes += nt_bytes as u64;
        self.name_hashes.push(name_hash);
    }
}

fn main() {
    let mut data_dir = String::from("/data");
    let mut out_dir = String::from("/out");

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--data" && i + 1 < args.len() {
            data_dir = args[i + 1].clone();
            i += 2;
        } else if args[i] == "--out" && i + 1 < args.len() {
            out_dir = args[i + 1].clone();
            i += 2;
        } else {
            i += 1;
        }
    }

    let regions_path = format!("{}/regions.csv", data_dir);
    let records_path = format!("{}/records.csv", data_dir);
    let out_path = format!("{}/aggregates.txt", out_dir);

    // 1. Read regions.csv
    let mut regions_map: HashMap<u32, String> = HashMap::with_capacity(1024);
    if let Ok(content) = std::fs::read_to_string(&regions_path) {
        for (idx, line) in content.lines().enumerate() {
            if idx == 0 {
                continue; // skip header
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(comma) = line.find(',') {
                if let Ok(reg_id) = line[..comma].trim().parse::<u32>() {
                    let country = line[comma + 1..].trim().to_string();
                    regions_map.insert(reg_id, country);
                }
            }
        }
    }

    // 2. Mmap records.csv
    let r_mmap = match FastMmap::open(&records_path) {
        Some(m) => m,
        None => {
            let _ = std::fs::create_dir_all(&out_dir);
            let _ = File::create(&out_path);
            return;
        }
    };

    let data = r_mmap.as_slice();
    let n = data.len();
    let mut p = 0;

    // Skip BOM if present at start
    if n >= 3 && data[0] == 0xEF && data[1] == 0xBB && data[2] == 0xBF {
        p = 3;
    }

    // Skip header line
    while p < n && data[p] != b'\n' {
        p += 1;
    }
    if p < n {
        p += 1;
    }

    let mut countries: HashMap<String, CountryAgg> = HashMap::with_capacity(256);
    let unmatched_str = String::from("_UNMATCHED");

    // Reusable field extraction buffer for unescaping quotes
    let mut unescaped = Vec::with_capacity(1024);

    while p < n {
        // Skip BOM if at record start
        if p + 3 <= n && data[p] == 0xEF && data[p + 1] == 0xBB && data[p + 2] == 0xBF {
            p += 3;
        }
        if p >= n || data[p] == b'\n' || data[p] == b'\r' {
            while p < n && (data[p] == b'\n' || data[p] == b'\r') {
                p += 1;
            }
            continue;
        }

        // Field 0: id (integer)
        let mut id: u64 = 0;
        while p < n && data[p] >= b'0' && data[p] <= b'9' {
            id = id * 10 + (data[p] - b'0') as u64;
            p += 1;
        }
        if p < n && data[p] == b',' {
            p += 1;
        }

        // Field 1: name
        let name_hash: u64;
        let name_bytes_len: usize;
        if p < n && data[p] == b'"' {
            p += 1; // skip opening quote
            unescaped.clear();
            while p < n {
                if data[p] == b'"' {
                    if p + 1 < n && data[p + 1] == b'"' {
                        unescaped.push(b'"');
                        p += 2;
                    } else {
                        p += 1; // closing quote
                        break;
                    }
                } else {
                    unescaped.push(data[p]);
                    p += 1;
                }
            }
            name_hash = fnv1a_64(&unescaped);
            name_bytes_len = unescaped.len();
        } else {
            let start = p;
            while p < n && data[p] != b',' && data[p] != b'\n' && data[p] != b'\r' {
                p += 1;
            }
            name_hash = fnv1a_64(&data[start..p]);
            name_bytes_len = p - start;
        }
        if p < n && data[p] == b',' {
            p += 1;
        }

        // Field 2: note
        let note_bytes_len: usize;
        if p < n && data[p] == b'"' {
            p += 1;
            let mut len = 0;
            while p < n {
                if data[p] == b'"' {
                    if p + 1 < n && data[p + 1] == b'"' {
                        len += 1;
                        p += 2;
                    } else {
                        p += 1;
                        break;
                    }
                } else {
                    len += 1;
                    p += 1;
                }
            }
            note_bytes_len = len;
        } else {
            let start = p;
            while p < n && data[p] != b',' && data[p] != b'\n' && data[p] != b'\r' {
                p += 1;
            }
            note_bytes_len = p - start;
        }
        if p < n && data[p] == b',' {
            p += 1;
        }

        // Field 3: region_id
        let mut region_id: u32 = 0;
        while p < n && data[p] >= b'0' && data[p] <= b'9' {
            region_id = region_id * 10 + (data[p] - b'0') as u32;
            p += 1;
        }
        if p < n && data[p] == b',' {
            p += 1;
        }

        // Field 4: amount
        let amt_start = p;
        while p < n && data[p] != b'\n' && data[p] != b'\r' {
            p += 1;
        }
        let cents = parse_cents(&data[amt_start..p]);

        // Advance past newline
        while p < n && (data[p] == b'\n' || data[p] == b'\r') {
            p += 1;
        }

        // Join on region_id
        let country_name = match regions_map.get(&region_id) {
            Some(c) => c,
            None => &unmatched_str,
        };

        let agg = countries.entry(country_name.clone()).or_insert_with(CountryAgg::new);
        agg.add(id, name_hash, name_bytes_len, note_bytes_len, cents);
    }

    // Sort countries by name ascending
    let sorted_countries: BTreeMap<String, CountryAgg> = countries.into_iter().collect();

    // Format output
    let mut out_buffer = Vec::with_capacity(1024 * 1024);
    for (country, mut agg) in sorted_countries {
        // Distinct names via sort + dedup on 64-bit hashes
        agg.name_hashes.sort_unstable();
        agg.name_hashes.dedup();
        let distinct_names = agg.name_hashes.len();

        let line = format!(
            "{} {} {} {} {} {} {} {} {}\n",
            country,
            agg.count,
            agg.id_sum,
            format_cents(agg.amount_sum),
            format_cents(if agg.amount_min == i64::MAX { 0 } else { agg.amount_min }),
            format_cents(if agg.amount_max == i64::MIN { 0 } else { agg.amount_max }),
            distinct_names,
            agg.name_bytes,
            agg.note_bytes
        );
        out_buffer.extend_from_slice(line.as_bytes());
    }

    let _ = std::fs::create_dir_all(&out_dir);
    if let Ok(mut f) = File::create(&out_path) {
        let _ = f.write_all(&out_buffer);
    }
}
