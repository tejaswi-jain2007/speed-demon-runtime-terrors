// Speed Demon 2026 - Track B (Index)
// Team: Runtime Terrors
// Ultra-fast zero-dependency search index engine in Rust

use std::collections::{BTreeSet, HashMap, HashSet};
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
fn is_token_char(b: u8) -> bool {
    (b >= b'a' && b <= b'z') || (b >= b'0' && b <= b'9') || (b >= b'A' && b <= b'Z')
}

#[inline(always)]
fn to_lower_ascii(b: u8) -> u8 {
    if b >= b'A' && b <= b'Z' {
        b + 32
    } else {
        b
    }
}

// Two-pointer set intersection of two sorted slices
fn intersect(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut res = Vec::with_capacity(a.len().min(b.len()));
    let mut i = 0;
    let mut j = 0;
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            res.push(a[i]);
            i += 1;
            j += 1;
        } else if a[i] < b[j] {
            i += 1;
        } else {
            j += 1;
        }
    }
    res
}

// Two-pointer set union of two sorted slices
fn union(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut res = Vec::with_capacity(a.len() + b.len());
    let mut i = 0;
    let mut j = 0;
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            res.push(a[i]);
            i += 1;
            j += 1;
        } else if a[i] < b[j] {
            res.push(a[i]);
            i += 1;
        } else {
            res.push(b[j]);
            j += 1;
        }
    }
    while i < a.len() {
        res.push(a[i]);
        i += 1;
    }
    while j < b.len() {
        res.push(b[j]);
        j += 1;
    }
    res
}

// Two-pointer set difference: a - b
fn difference(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut res = Vec::with_capacity(a.len());
    let mut i = 0;
    let mut j = 0;
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            i += 1;
            j += 1;
        } else if a[i] < b[j] {
            res.push(a[i]);
            i += 1;
        } else {
            j += 1;
        }
    }
    while i < a.len() {
        res.push(a[i]);
        i += 1;
    }
    res
}

fn tokenize_slice(text: &[u8]) -> Vec<Vec<u8>> {
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < text.len() {
        while i < text.len() && !is_token_char(text[i]) {
            i += 1;
        }
        if i >= text.len() {
            break;
        }
        let start = i;
        while i < text.len() && is_token_char(text[i]) {
            i += 1;
        }
        let mut tok = text[start..i].to_vec();
        for b in &mut tok {
            *b = to_lower_ascii(*b);
        }
        tokens.push(tok);
    }
    tokens
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

    let docs_path = format!("{}/docs.txt", data_dir);
    let queries_path = format!("{}/queries.txt", data_dir);
    let out_path = format!("{}/results.txt", out_dir);

    // 1. Read queries.txt first to harvest only terms that are actually queried!
    let q_mmap = match FastMmap::open(&queries_path) {
        Some(m) => m,
        None => {
            let _ = std::fs::create_dir_all(&out_dir);
            let _ = File::create(&out_path);
            return;
        }
    };

    let q_data = q_mmap.as_slice();
    let mut query_lines: Vec<&[u8]> = Vec::new();
    let mut query_terms: HashSet<Vec<u8>> = HashSet::new();

    let mut q_start = 0;
    for (idx, &b) in q_data.iter().enumerate() {
        if b == b'\n' {
            let mut line = &q_data[q_start..idx];
            if !line.is_empty() && line[line.len() - 1] == b'\r' {
                line = &line[..line.len() - 1];
            }
            if !line.is_empty() {
                query_lines.push(line);
                // Extract terms
                if line.starts_with(b"BOOL ") {
                    let parts: Vec<&[u8]> = line[5..].split(|&c| c == b' ').collect();
                    let mut k = 0;
                    while k < parts.len() {
                        let mut term = parts[k].to_vec();
                        for byte in &mut term {
                            *byte = to_lower_ascii(*byte);
                        }
                        query_terms.insert(term);
                        k += 2; // skip operator
                    }
                } else if line.starts_with(b"PHRASE \"") && line.ends_with(b"\"") {
                    let inner = &line[8..line.len() - 1];
                    for word in inner.split(|&c| c == b' ') {
                        let mut term = word.to_vec();
                        for byte in &mut term {
                            *byte = to_lower_ascii(*byte);
                        }
                        query_terms.insert(term);
                    }
                }
            }
            q_start = idx + 1;
        }
    }

    // 2. Mmap docs.txt and index only vocabulary present in queries!
    let d_mmap = match FastMmap::open(&docs_path) {
        Some(m) => m,
        None => {
            let _ = std::fs::create_dir_all(&out_dir);
            let _ = File::create(&out_path);
            return;
        }
    };

    let d_data = d_mmap.as_slice();
    let mut doc_spans: Vec<(usize, usize)> = Vec::with_capacity(128 * 1024);
    let mut index: HashMap<Vec<u8>, Vec<u32>> = HashMap::with_capacity(query_terms.len());

    let mut line_start = 0;
    let mut cur_doc_id: u32 = 0;
    let mut seen_terms: HashSet<Vec<u8>> = HashSet::with_capacity(256);

    for (idx, &b) in d_data.iter().enumerate() {
        if b == b'\n' {
            let mut line = &d_data[line_start..idx];
            if !line.is_empty() && line[line.len() - 1] == b'\r' {
                line = &line[..line.len() - 1];
            }
            if let Some(tab_pos) = line.iter().position(|&c| c == b'\t') {
                let text_start = line_start + tab_pos + 1;
                let text_len = idx - text_start;
                doc_spans.push((text_start, text_len));

                seen_terms.clear();
                let text = &d_data[text_start..text_start + text_len];
                let mut p = 0;
                while p < text.len() {
                    while p < text.len() && !is_token_char(text[p]) {
                        p += 1;
                    }
                    if p >= text.len() {
                        break;
                    }
                    let t_start = p;
                    while p < text.len() && is_token_char(text[p]) {
                        p += 1;
                    }
                    let mut tok = text[t_start..p].to_vec();
                    for byte in &mut tok {
                        *byte = to_lower_ascii(*byte);
                    }

                    if query_terms.contains(&tok) && seen_terms.insert(tok.clone()) {
                        index.entry(tok).or_default().push(cur_doc_id);
                    }
                }
                cur_doc_id += 1;
            }
            line_start = idx + 1;
        }
    }

    // 3. Process all queries
    let empty_vec: Vec<u32> = Vec::new();
    let mut out_buffer = Vec::with_capacity(2 * 1024 * 1024);

    for line in query_lines {
        if line.starts_with(b"BOOL ") {
            let parts: Vec<&[u8]> = line[5..].split(|&c| c == b' ').collect();
            let mut term0 = parts[0].to_vec();
            for byte in &mut term0 {
                *byte = to_lower_ascii(*byte);
            }

            let mut running = index.get(&term0).cloned().unwrap_or_default();
            let mut k = 1;
            while k + 1 < parts.len() {
                let op = parts[k];
                let mut term_k = parts[k + 1].to_vec();
                for byte in &mut term_k {
                    *byte = to_lower_ascii(*byte);
                }
                let postings = index.get(&term_k).unwrap_or(&empty_vec);

                if op == b"AND" {
                    running = intersect(&running, postings);
                } else if op == b"OR" {
                    running = union(&running, postings);
                } else if op == b"NOT" {
                    running = difference(&running, postings);
                }
                k += 2;
            }

            // Format result: count sum_of_ids id0 id1 ... id9
            let count = running.len();
            let sum: u64 = running.iter().map(|&x| x as u64).sum();
            out_buffer.extend_from_slice(count.to_string().as_bytes());
            out_buffer.push(b' ');
            out_buffer.extend_from_slice(sum.to_string().as_bytes());
            let take = count.min(10);
            for &id in &running[..take] {
                out_buffer.push(b' ');
                out_buffer.extend_from_slice(id.to_string().as_bytes());
            }
            out_buffer.push(b'\n');
        } else if line.starts_with(b"PHRASE \"") && line.ends_with(b"\"") {
            let inner = &line[8..line.len() - 1];
            let words: Vec<Vec<u8>> = inner.split(|&c| c == b' ').map(|w| {
                let mut tok = w.to_vec();
                for byte in &mut tok {
                    *byte = to_lower_ascii(*byte);
                }
                tok
            }).collect();

            if words.is_empty() {
                out_buffer.extend_from_slice(b"0 0\n");
                continue;
            }

            // Candidates = intersection of all terms
            let mut candidates = index.get(&words[0]).cloned().unwrap_or_default();
            for w in &words[1..] {
                if candidates.is_empty() {
                    break;
                }
                let postings = index.get(w).unwrap_or(&empty_vec);
                candidates = intersect(&candidates, postings);
            }

            // Verify phrase across candidates
            let mut matches: Vec<u32> = Vec::new();
            let k = words.len();
            for doc_id in candidates {
                let (span_start, span_len) = doc_spans[doc_id as usize];
                let text = &d_data[span_start..span_start + span_len];
                let tokens = tokenize_slice(text);
                let mut found = false;
                if tokens.len() >= k {
                    for start in 0..=(tokens.len() - k) {
                        if tokens[start..start + k] == words[..] {
                            found = true;
                            break;
                        }
                    }
                }
                if found {
                    matches.push(doc_id);
                }
            }

            let count = matches.len();
            let sum: u64 = matches.iter().map(|&x| x as u64).sum();
            out_buffer.extend_from_slice(count.to_string().as_bytes());
            out_buffer.push(b' ');
            out_buffer.extend_from_slice(sum.to_string().as_bytes());
            let take = count.min(10);
            for &id in &matches[..take] {
                out_buffer.push(b' ');
                out_buffer.extend_from_slice(id.to_string().as_bytes());
            }
            out_buffer.push(b'\n');
        }
    }

    let _ = std::fs::create_dir_all(&out_dir);
    if let Ok(mut f) = File::create(&out_path) {
        let _ = f.write_all(&out_buffer);
    }
}
