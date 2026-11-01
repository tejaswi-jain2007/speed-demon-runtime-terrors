// Track B - Multi-Threaded High-Performance Index in Rust
use std::fs::File;
use std::io::Write;
use std::arch::x86_64::*;
use std::os::unix::io::AsRawFd;
use std::thread;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    fn prctl(option: i32, a2: u64, a3: u64, a4: u64, a5: u64) -> i32;
    fn _exit(status: i32) -> !;
}

const NTHREADS: usize = 12;
const TABLE_CAP: usize = 65536;

#[inline(always)]
fn is_token_char(c: u8) -> bool {
    (c >= b'a' && c <= b'z') || (c >= b'0' && c <= b'9') || (c >= b'A' && c <= b'Z')
}

#[inline(always)]
fn to_lower(c: u8) -> u8 {
    if c >= b'A' && c <= b'Z' { c + 32 } else { c }
}

#[inline(always)]
unsafe fn hash_token(p: *const u8, len: usize) -> u64 {
    let mut h = len as u64;
    let mut rem = len;
    let mut ptr = p;
    while rem >= 8 {
        let v = (ptr as *const u64).read_unaligned();
        h = _mm_crc32_u64(h, v);
        ptr = ptr.add(8); rem -= 8;
    }
    if rem >= 4 {
        let v = (ptr as *const u32).read_unaligned();
        h = _mm_crc32_u32(h as u32, v) as u64;
        ptr = ptr.add(4); rem -= 4;
    }
    while rem > 0 {
        h = _mm_crc32_u8(h as u32, *ptr) as u64;
        ptr = ptr.add(1); rem -= 1;
    }
    h
}

#[derive(Copy, Clone)]
struct TermSlot {
    fp: u64,
    term_id: u32,
    len: u16,
    ptr: *const u8,
}

struct TermTable {
    slots: Vec<TermSlot>,
}

unsafe impl Send for TermTable {}
unsafe impl Sync for TermTable {}

impl TermTable {
    fn new() -> Self {
        Self {
            slots: vec![TermSlot { fp: 0, term_id: u32::MAX, len: 0, ptr: std::ptr::null() }; TABLE_CAP],
        }
    }

    #[inline(always)]
    unsafe fn insert(&mut self, text: &[u8], tid: u32) {
        let h = hash_token(text.as_ptr(), text.len());
        let fp = h | 1;
        let mut idx = (h as usize) & (TABLE_CAP - 1);
        loop {
            let s = self.slots.get_unchecked_mut(idx);
            if s.fp == 0 {
                s.fp = fp;
                s.term_id = tid;
                s.len = text.len() as u16;
                s.ptr = text.as_ptr();
                return;
            }
            if s.fp == fp && s.len as usize == text.len() && std::slice::from_raw_parts(s.ptr, text.len()) == text {
                return;
            }
            idx = (idx + 1) & (TABLE_CAP - 1);
        }
    }

    #[inline(always)]
    unsafe fn lookup(&self, text: *const u8, len: usize) -> Option<u32> {
        let h = hash_token(text, len);
        let fp = h | 1;
        let mut idx = (h as usize) & (TABLE_CAP - 1);
        loop {
            let s = self.slots.get_unchecked(idx);
            if s.fp == 0 {
                return None;
            }
            if s.fp == fp && s.len as usize == len && std::slice::from_raw_parts(s.ptr, len) == std::slice::from_raw_parts(text, len) {
                return Some(s.term_id);
            }
            idx = (idx + 1) & (TABLE_CAP - 1);
        }
    }
}

enum Query {
    Bool(Vec<(u8, u32)>),
    Phrase(Vec<u32>),
}

#[derive(Copy, Clone)]
struct DocSpan {
    offset: u32,
    len: u32,
}

fn main() {
    unsafe { prctl(14, 1, 0, 0, 0); } // PR_SET_THP_DISABLE = 14
    let (mut data_dir, mut out_dir) = (String::from("/data"), String::from("/out"));
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--data" && i + 1 < args.len() { data_dir = args[i + 1].clone(); i += 2; }
        else if args[i] == "--out" && i + 1 < args.len() { out_dir = args[i + 1].clone(); i += 2; }
        else { i += 1; }
    }

    let queries_path = format!("{}/queries.txt", data_dir);
    let docs_path = format!("{}/docs.txt", data_dir);
    let out_path = format!("{}/results.txt", out_dir);

    // 1. Read queries.txt
    let q_content = match std::fs::read(&queries_path) { Ok(c) => c, Err(_) => return };
    
    let mut term_map = std::collections::HashMap::<Vec<u8>, u32>::new();
    let mut term_names = Vec::<Vec<u8>>::new();

    let mut get_term_id = |term: &[u8]| -> u32 {
        if let Some(&id) = term_map.get(term) {
            id
        } else {
            let id = term_names.len() as u32;
            term_map.insert(term.to_vec(), id);
            term_names.push(term.to_vec());
            id
        }
    };

    let mut queries = Vec::new();
    let mut line_start = 0;
    while line_start < q_content.len() {
        let mut line_end = line_start;
        while line_end < q_content.len() && q_content[line_end] != b'\n' { line_end += 1; }
        let line = &q_content[line_start..line_end];
        line_start = line_end + 1;
        if line.is_empty() { continue; }

        if line.starts_with(b"BOOL ") {
            let parts: Vec<&[u8]> = line[5..].split(|&c| c == b' ').filter(|s| !s.is_empty()).collect();
            if parts.is_empty() {
                queries.push(Query::Bool(Vec::new()));
                continue;
            }
            let mut steps = Vec::new();
            steps.push((0u8, get_term_id(parts[0])));
            let mut pi = 1;
            while pi + 1 < parts.len() {
                let op = match parts[pi] {
                    b"AND" => 1u8,
                    b"OR" => 2u8,
                    b"NOT" => 3u8,
                    _ => 1u8,
                };
                let tid = get_term_id(parts[pi + 1]);
                steps.push((op, tid));
                pi += 2;
            }
            queries.push(Query::Bool(steps));
        } else if line.starts_with(b"PHRASE \"") {
            let end_quote = line.iter().rposition(|&c| c == b'"').unwrap_or(line.len());
            if end_quote > 8 {
                let phrase_str = &line[8..end_quote];
                let terms: Vec<u32> = phrase_str.split(|&c| c == b' ').filter(|s| !s.is_empty()).map(|t| get_term_id(t)).collect();
                queries.push(Query::Phrase(terms));
            } else {
                queries.push(Query::Phrase(Vec::new()));
            }
        }
    }

    let mut term_table = TermTable::new();
    for (tid, name) in term_names.iter().enumerate() {
        unsafe { term_table.insert(name, tid as u32); }
    }

    // 2. Open and mmap docs.txt (MAP_SHARED, read-only: ZERO dirty pages!)
    let d_file = match File::open(&docs_path) { Ok(f) => f, Err(_) => return };
    let d_size = d_file.metadata().map(|m| m.len() as usize).unwrap_or(0);
    if d_size == 0 { return; }

    let d_ptr = unsafe {
        mmap(std::ptr::null_mut(), d_size, 1, 1, d_file.as_raw_fd(), 0) // PROT_READ, MAP_SHARED
    };
    if d_ptr.is_null() || d_ptr as isize == -1 { return; }
    unsafe { madvise(d_ptr, d_size, 2); } // MADV_SEQUENTIAL

    let d_slice = unsafe { std::slice::from_raw_parts(d_ptr, d_size) };

    // Find thread chunk boundaries by LF
    let mut splits = vec![0usize; NTHREADS + 1];
    splits[NTHREADS] = d_size;
    for t in 1..NTHREADS {
        let mut pt = t * (d_size / NTHREADS);
        while pt < d_size && d_slice[pt] != b'\n' { pt += 1; }
        if pt < d_size && d_slice[pt] == b'\n' { pt += 1; }
        splits[t] = pt.min(d_size);
    }

    let num_terms = term_names.len();
    let mut thread_postings = vec![vec![Vec::<u32>::new(); num_terms]; NTHREADS];
    let mut thread_docs = vec![Vec::<DocSpan>::new(); NTHREADS];

    thread::scope(|s| {
        let t_table = &term_table;
        let d_bytes = d_slice;
        let splits_ref = &splits;
        let post_slice = &mut thread_postings[..];
        let docs_slice = &mut thread_docs[..];

        for (t, (t_postings, t_docs)) in post_slice.iter_mut().zip(docs_slice.iter_mut()).enumerate() {
            let (start, end) = (splits_ref[t], splits_ref[t + 1]);

            s.spawn(move || {
                let mut pos = start;
                let mut doc_tokens = Vec::<u32>::with_capacity(128);
                let mut token_buf = [0u8; 64];

                while pos < end {
                    // Parse doc ID
                    let mut doc_id = 0u32;
                    while pos < end && d_bytes[pos] >= b'0' && d_bytes[pos] <= b'9' {
                        doc_id = doc_id * 10 + (d_bytes[pos] - b'0') as u32;
                        pos += 1;
                    }
                    if pos < end && d_bytes[pos] == b'\t' { pos += 1; }

                    let text_start = pos;
                    while pos < end && d_bytes[pos] != b'\n' { pos += 1; }
                    let text_end = pos;
                    if pos < end && d_bytes[pos] == b'\n' { pos += 1; }

                    if (doc_id as usize) >= t_docs.len() {
                        t_docs.resize(doc_id as usize + 1, DocSpan { offset: 0, len: 0 });
                    }
                    t_docs[doc_id as usize] = DocSpan {
                        offset: text_start as u32,
                        len: (text_end - text_start) as u32,
                    };

                    doc_tokens.clear();
                    let mut tp = text_start;
                    while tp < text_end {
                        if is_token_char(d_bytes[tp]) {
                            let mut tlen = 0;
                            while tp < text_end && is_token_char(d_bytes[tp]) {
                                if tlen < 64 {
                                    token_buf[tlen] = to_lower(d_bytes[tp]);
                                    tlen += 1;
                                }
                                tp += 1;
                            }
                            if let Some(tid) = unsafe { t_table.lookup(token_buf.as_ptr(), tlen) } {
                                doc_tokens.push(tid);
                            }
                        } else {
                            tp += 1;
                        }
                    }

                    doc_tokens.sort_unstable();
                    doc_tokens.dedup();
                    for &tid in &doc_tokens {
                        t_postings[tid as usize].push(doc_id);
                    }
                }
            });
        }
    });

    // Merge doc spans
    let mut total_docs = 0;
    for td in &thread_docs {
        total_docs = total_docs.max(td.len());
    }
    let mut docs = vec![DocSpan { offset: 0, len: 0 }; total_docs];
    for td in &thread_docs {
        for (i, span) in td.iter().enumerate() {
            if span.len > 0 {
                docs[i] = *span;
            }
        }
    }
    drop(thread_docs);

    // Merge postings across threads (since each thread has non-overlapping ascending doc IDs, simply concatenate!)
    let mut postings: Vec<Vec<u32>> = Vec::with_capacity(num_terms);
    for tid in 0..num_terms {
        let mut total_len = 0;
        for t in 0..NTHREADS {
            total_len += thread_postings[t][tid].len();
        }
        let mut p = Vec::with_capacity(total_len);
        for t in 0..NTHREADS {
            p.extend_from_slice(&thread_postings[t][tid]);
        }
        postings.push(p);
    }
    drop(thread_postings);

    // 3. Parallel Query Execution across 12 threads
    let num_queries = queries.len();
    let q_per_thread = (num_queries + NTHREADS - 1) / NTHREADS;
    let mut thread_results = vec![Vec::<String>::new(); NTHREADS];

    thread::scope(|s| {
        let queries_ref = &queries;
        let postings_ref = &postings;
        let docs_ref = &docs;
        let d_bytes = d_slice;
        let term_names_ref = &term_names;
        let res_slice = &mut thread_results[..];

        for (t, out_res) in res_slice.iter_mut().enumerate() {
            let start_q = t * q_per_thread;
            let end_q = (start_q + q_per_thread).min(num_queries);

            s.spawn(move || {
                if start_q >= end_q { return; }
                out_res.reserve(end_q - start_q);

                let op_and = |a: &[u32], b: &[u32], out: &mut Vec<u32>| {
                    out.clear();
                    let (mut i, mut j) = (0, 0);
                    while i < a.len() && j < b.len() {
                        if a[i] == b[j] { out.push(a[i]); i += 1; j += 1; }
                        else if a[i] < b[j] { i += 1; }
                        else { j += 1; }
                    }
                };

                let op_or = |a: &[u32], b: &[u32], out: &mut Vec<u32>| {
                    out.clear();
                    let (mut i, mut j) = (0, 0);
                    while i < a.len() && j < b.len() {
                        if a[i] == b[j] { out.push(a[i]); i += 1; j += 1; }
                        else if a[i] < b[j] { out.push(a[i]); i += 1; }
                        else { out.push(b[j]); j += 1; }
                    }
                    out.extend_from_slice(&a[i..]);
                    out.extend_from_slice(&b[j..]);
                };

                let op_not = |a: &[u32], b: &[u32], out: &mut Vec<u32>| {
                    out.clear();
                    let (mut i, mut j) = (0, 0);
                    while i < a.len() && j < b.len() {
                        if a[i] == b[j] { i += 1; j += 1; }
                        else if a[i] < b[j] { out.push(a[i]); i += 1; }
                        else { j += 1; }
                    }
                    out.extend_from_slice(&a[i..]);
                };

                let format_result = |ids: &[u32]| -> String {
                    if ids.is_empty() { return String::from("0 0"); }
                    let sum: u64 = ids.iter().map(|&x| x as u64).sum();
                    let mut s = format!("{} {}", ids.len(), sum);
                    let limit = ids.len().min(10);
                    for &id in &ids[..limit] {
                        s.push(' ');
                        s.push_str(&id.to_string());
                    }
                    s
                };

                let mut buf_a = Vec::<u32>::new();
                let mut buf_b = Vec::<u32>::new();
                let mut token_buf = [0u8; 64];
                let mut doc_words = Vec::<Vec<u8>>::new();

                for qi in start_q..end_q {
                    match &queries_ref[qi] {
                        Query::Bool(steps) => {
                            if steps.is_empty() {
                                out_res.push(String::from("0 0"));
                                continue;
                            }
                            buf_a.clear();
                            buf_a.extend_from_slice(&postings_ref[steps[0].1 as usize]);
                            for &(op, tid) in &steps[1..] {
                                let post = &postings_ref[tid as usize];
                                if op == 1 { op_and(&buf_a, post, &mut buf_b); }
                                else if op == 2 { op_or(&buf_a, post, &mut buf_b); }
                                else if op == 3 { op_not(&buf_a, post, &mut buf_b); }
                                std::mem::swap(&mut buf_a, &mut buf_b);
                            }
                            out_res.push(format_result(&buf_a));
                        }
                        Query::Phrase(p_terms) => {
                            if p_terms.is_empty() {
                                out_res.push(String::from("0 0"));
                                continue;
                            }
                            buf_a.clear();
                            buf_a.extend_from_slice(&postings_ref[p_terms[0] as usize]);
                            for &tid in &p_terms[1..] {
                                let post = &postings_ref[tid as usize];
                                op_and(&buf_a, post, &mut buf_b);
                                std::mem::swap(&mut buf_a, &mut buf_b);
                                if buf_a.is_empty() { break; }
                            }

                            let mut matches = Vec::new();
                            let k = p_terms.len();
                            let phrase_strings: Vec<&[u8]> = p_terms.iter().map(|&tid| term_names_ref[tid as usize].as_slice()).collect();

                            for &doc_id in &buf_a {
                                let span = &docs_ref[doc_id as usize];
                                if span.len == 0 { continue; }
                                let text = &d_bytes[span.offset as usize..(span.offset + span.len) as usize];

                                doc_words.clear();
                                let mut tp = 0;
                                while tp < text.len() {
                                    if is_token_char(text[tp]) {
                                        let mut tlen = 0;
                                        while tp < text.len() && is_token_char(text[tp]) {
                                            if tlen < 64 {
                                                token_buf[tlen] = to_lower(text[tp]);
                                                tlen += 1;
                                            }
                                            tp += 1;
                                        }
                                        doc_words.push(token_buf[..tlen].to_vec());
                                    } else {
                                        tp += 1;
                                    }
                                }

                                if doc_words.len() < k { continue; }
                                let mut found = false;
                                for i in 0..=doc_words.len() - k {
                                    if doc_words[i..i+k] == phrase_strings[..] {
                                        found = true;
                                        break;
                                    }
                                }
                                if found { matches.push(doc_id); }
                            }
                            out_res.push(format_result(&matches));
                        }
                    }
                }
            });
        }
    });

    let _ = std::fs::create_dir_all(&out_dir);
    if let Ok(mut f) = File::create(&out_path) {
        for tr in &thread_results {
            for r in tr {
                let _ = writeln!(f, "{}", r);
            }
        }
    }

    unsafe { _exit(0); }
}
