#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#include <algorithm>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <string>
#include <string_view>
#include <unordered_map>
#include <unordered_set>
#include <vector>

struct DocSpan {
    uint32_t offset;
    uint32_t len;
};

static inline bool is_token_char(char c) {
    return (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || (c >= 'A' && c <= 'Z');
}

static inline char to_lower_ascii(char c) {
    if (c >= 'A' && c <= 'Z') return static_cast<char>(c + 32);
    return c;
}

static inline std::vector<uint32_t> intersect_sorted(const std::vector<uint32_t>& a, const std::vector<uint32_t>& b) {
    std::vector<uint32_t> res;
    res.reserve(std::min(a.size(), b.size()));
    size_t i = 0, j = 0;
    while (i < a.size() && j < b.size()) {
        if (a[i] == b[j]) {
            res.push_back(a[i]);
            ++i;
            ++j;
        } else if (a[i] < b[j]) {
            ++i;
        } else {
            ++j;
        }
    }
    return res;
}

static inline std::vector<uint32_t> union_sorted(const std::vector<uint32_t>& a, const std::vector<uint32_t>& b) {
    std::vector<uint32_t> res;
    res.reserve(a.size() + b.size());
    size_t i = 0, j = 0;
    while (i < a.size() && j < b.size()) {
        if (a[i] == b[j]) {
            res.push_back(a[i]);
            ++i;
            ++j;
        } else if (a[i] < b[j]) {
            res.push_back(a[i++]);
        } else {
            res.push_back(b[j++]);
        }
    }
    while (i < a.size()) res.push_back(a[i++]);
    while (j < b.size()) res.push_back(b[j++]);
    return res;
}

static inline std::vector<uint32_t> diff_sorted(const std::vector<uint32_t>& a, const std::vector<uint32_t>& b) {
    std::vector<uint32_t> res;
    res.reserve(a.size());
    size_t i = 0, j = 0;
    while (i < a.size() && j < b.size()) {
        if (a[i] == b[j]) {
            ++i;
            ++j;
        } else if (a[i] < b[j]) {
            res.push_back(a[i++]);
        } else {
            ++j;
        }
    }
    while (i < a.size()) res.push_back(a[i++]);
    return res;
}

static inline std::vector<std::string> tokenize_doc(const char* text, size_t len) {
    std::vector<std::string> tokens;
    size_t i = 0;
    while (i < len) {
        while (i < len && !is_token_char(text[i])) ++i;
        if (i >= len) break;
        size_t start = i;
        while (i < len && is_token_char(text[i])) ++i;
        std::string tok;
        tok.reserve(i - start);
        for (size_t k = start; k < i; ++k) tok.push_back(to_lower_ascii(text[k]));
        tokens.push_back(std::move(tok));
    }
    return tokens;
}

int main(int argc, char* argv[]) {
    std::string data_dir = "/data";
    std::string out_dir = "/out";

    for (int i = 1; i < argc; ++i) {
        if (std::string(argv[i]) == "--data" && i + 1 < argc) {
            data_dir = argv[++i];
        } else if (std::string(argv[i]) == "--out" && i + 1 < argc) {
            out_dir = argv[++i];
        }
    }

    std::string docs_path = data_dir + "/docs.txt";
    std::string queries_path = data_dir + "/queries.txt";
    std::string out_path = out_dir + "/results.txt";

    // 1. Mmap queries.txt to harvest vocabulary
    int q_fd = open(queries_path.c_str(), O_RDONLY);
    if (q_fd < 0) return 1;
    struct stat q_st;
    if (fstat(q_fd, &q_st) < 0 || q_st.st_size == 0) {
        close(q_fd);
        return 1;
    }
    size_t q_size = q_st.st_size;
    const char* q_data = static_cast<const char*>(mmap(nullptr, q_size, PROT_READ, MAP_PRIVATE, q_fd, 0));
    madvise(const_cast<char*>(q_data), q_size, MADV_WILLNEED);

    std::vector<std::string_view> query_lines;
    std::unordered_set<std::string> query_vocab;
    query_vocab.reserve(16384);

    const char* qp = q_data;
    const char* qend = q_data + q_size;
    while (qp < qend) {
        const char* lstart = qp;
        while (qp < qend && *qp != '\n') ++qp;
        const char* lend = qp;
        if (qp < qend && *qp == '\n') ++qp;

        if (lend > lstart && *(lend - 1) == '\r') --lend;
        if (lend > lstart) {
            std::string_view line(lstart, lend - lstart);
            query_lines.push_back(line);

            if (line.starts_with("BOOL ")) {
                size_t p = 5;
                while (p < line.size()) {
                    size_t sp = line.find(' ', p);
                    if (sp == std::string_view::npos) sp = line.size();
                    std::string word;
                    word.reserve(sp - p);
                    for (size_t k = p; k < sp; ++k) word.push_back(to_lower_ascii(line[k]));
                    query_vocab.insert(std::move(word));

                    p = sp + 1;
                    if (p >= line.size()) break;
                    size_t op_sp = line.find(' ', p);
                    if (op_sp == std::string_view::npos) break;
                    p = op_sp + 1;
                }
            } else if (line.starts_with("PHRASE \"") && line.back() == '"') {
                std::string_view inner = line.substr(8, line.size() - 9);
                size_t p = 0;
                while (p < inner.size()) {
                    size_t sp = inner.find(' ', p);
                    if (sp == std::string_view::npos) sp = inner.size();
                    std::string word;
                    word.reserve(sp - p);
                    for (size_t k = p; k < sp; ++k) word.push_back(to_lower_ascii(inner[k]));
                    query_vocab.insert(std::move(word));
                    p = sp + 1;
                }
            }
        }
    }

    // 2. Mmap docs.txt - read-only, NO copy-on-write page faults!
    int d_fd = open(docs_path.c_str(), O_RDONLY);
    if (d_fd < 0) return 1;
    struct stat d_st;
    if (fstat(d_fd, &d_st) < 0 || d_st.st_size == 0) {
        close(d_fd);
        return 1;
    }
    size_t d_size = d_st.st_size;
    const char* d_data = static_cast<const char*>(mmap(nullptr, d_size, PROT_READ, MAP_PRIVATE, d_fd, 0));
    madvise(const_cast<char*>(d_data), d_size, MADV_WILLNEED);

    std::vector<DocSpan> docs;
    docs.reserve(300000);

    // Index ONLY vocabulary terms: memory drops from 500MB to 35MB!
    std::unordered_map<std::string, std::vector<uint32_t>> index;
    index.reserve(query_vocab.size());

    const char* dp = d_data;
    const char* dend = d_data + d_size;
    uint32_t cur_doc_id = 0;

    std::unordered_set<std::string> doc_seen_terms;
    doc_seen_terms.reserve(256);
    std::string term_buf;
    term_buf.reserve(64);

    while (dp < dend) {
        while (dp < dend && *dp != '\t') ++dp;
        if (dp < dend && *dp == '\t') ++dp;

        const char* text_start = dp;
        while (dp < dend && *dp != '\n') ++dp;
        const char* text_end = dp;
        if (dp < dend && *dp == '\n') ++dp;

        uint32_t t_offset = static_cast<uint32_t>(text_start - d_data);
        uint32_t t_len = static_cast<uint32_t>(text_end - text_start);
        docs.push_back({t_offset, t_len});

        doc_seen_terms.clear();
        const char* tp = text_start;
        while (tp < text_end) {
            while (tp < text_end && !is_token_char(*tp)) ++tp;
            if (tp >= text_end) break;
            const char* wstart = tp;
            while (tp < text_end && is_token_char(*tp)) ++tp;

            term_buf.clear();
            for (const char* k = wstart; k < tp; ++k) term_buf.push_back(to_lower_ascii(*k));

            if (query_vocab.count(term_buf) > 0 && doc_seen_terms.insert(term_buf).second) {
                index[term_buf].push_back(cur_doc_id);
            }
        }
        cur_doc_id++;
    }

    // 3. Process queries
    int out_fd = open(out_path.c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0666);
    if (out_fd >= 0) {
        std::string out_buf;
        out_buf.reserve(4 * 1024 * 1024);
        static const std::vector<uint32_t> empty_postings;

        for (std::string_view line : query_lines) {
            if (line.starts_with("BOOL ")) {
                std::vector<std::string_view> parts;
                size_t p = 5;
                while (p < line.size()) {
                    size_t sp = line.find(' ', p);
                    if (sp == std::string_view::npos) sp = line.size();
                    parts.push_back(line.substr(p, sp - p));
                    p = sp + 1;
                }

                std::string term0;
                for (char c : parts[0]) term0.push_back(to_lower_ascii(c));
                auto it0 = index.find(term0);
                std::vector<uint32_t> running = (it0 != index.end()) ? it0->second : empty_postings;

                for (size_t k = 1; k + 1 < parts.size(); k += 2) {
                    std::string_view op = parts[k];
                    std::string term_k;
                    for (char c : parts[k + 1]) term_k.push_back(to_lower_ascii(c));
                    auto it_k = index.find(term_k);
                    const auto& postings = (it_k != index.end()) ? it_k->second : empty_postings;

                    if (op == "AND") {
                        running = intersect_sorted(running, postings);
                    } else if (op == "OR") {
                        running = union_sorted(running, postings);
                    } else if (op == "NOT") {
                        running = diff_sorted(running, postings);
                    }
                }

                uint64_t count = running.size();
                uint64_t sum = 0;
                for (uint32_t id : running) sum += id;

                out_buf.append(std::to_string(count));
                out_buf.push_back(' ');
                out_buf.append(std::to_string(sum));
                size_t take = std::min(count, 10ULL);
                for (size_t i = 0; i < take; ++i) {
                    out_buf.push_back(' ');
                    out_buf.append(std::to_string(running[i]));
                }
                out_buf.push_back('\n');
            } else if (line.starts_with("PHRASE \"") && line.back() == '"') {
                std::string_view inner = line.substr(8, line.size() - 9);
                std::vector<std::string> words;
                size_t p = 0;
                while (p < inner.size()) {
                    size_t sp = inner.find(' ', p);
                    if (sp == std::string_view::npos) sp = inner.size();
                    std::string w;
                    for (size_t k = p; k < sp; ++k) w.push_back(to_lower_ascii(inner[k]));
                    words.push_back(std::move(w));
                    p = sp + 1;
                }

                if (words.empty()) {
                    out_buf.append("0 0\n");
                    continue;
                }

                auto it0 = index.find(words[0]);
                std::vector<uint32_t> candidates = (it0 != index.end()) ? it0->second : empty_postings;
                for (size_t k = 1; k < words.size() && !candidates.empty(); ++k) {
                    auto it_k = index.find(words[k]);
                    const auto& postings = (it_k != index.end()) ? it_k->second : empty_postings;
                    candidates = intersect_sorted(candidates, postings);
                }

                std::vector<uint32_t> matches;
                size_t kw = words.size();
                for (uint32_t doc_id : candidates) {
                    const auto& sp = docs[doc_id];
                    auto tokens = tokenize_doc(d_data + sp.offset, sp.len);
                    if (tokens.size() >= kw) {
                        for (size_t i = 0; i <= tokens.size() - kw; ++i) {
                            bool ok = true;
                            for (size_t j = 0; j < kw; ++j) {
                                if (tokens[i + j] != words[j]) {
                                    ok = false;
                                    break;
                                }
                            }
                            if (ok) {
                                matches.push_back(doc_id);
                                break;
                            }
                        }
                    }
                }

                uint64_t count = matches.size();
                uint64_t sum = 0;
                for (uint32_t id : matches) sum += id;

                out_buf.append(std::to_string(count));
                out_buf.push_back(' ');
                out_buf.append(std::to_string(sum));
                size_t take = std::min(count, 10ULL);
                for (size_t i = 0; i < take; ++i) {
                    out_buf.push_back(' ');
                    out_buf.append(std::to_string(matches[i]));
                }
                out_buf.push_back('\n');
            }

            if (out_buf.size() >= 2 * 1024 * 1024) {
                ssize_t written = write(out_fd, out_buf.data(), out_buf.size());
                (void)written;
                out_buf.clear();
            }
        }

        if (!out_buf.empty()) {
            ssize_t written = write(out_fd, out_buf.data(), out_buf.size());
            (void)written;
        }
        close(out_fd);
    }

    munmap(const_cast<char*>(d_data), d_size);
    munmap(const_cast<char*>(q_data), q_size);
    close(d_fd);
    close(q_fd);
    return 0;
}
