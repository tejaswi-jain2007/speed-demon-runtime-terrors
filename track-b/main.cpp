#include <iostream>
#include <fstream>
#include <vector>
#include <string>
#include <string_view>
#include <unordered_map>
#include <algorithm>
#include <thread>
#include <cstdint>
#include <cstring>
#include <sys/mman.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>

struct TokenSpan {
    uint32_t start;
    uint32_t len;
};

// Fast custom string hash
struct StrViewHash {
    size_t operator()(std::string_view sv) const {
        size_t h = 2166136261u;
        for (char c : sv) {
            h = (h ^ static_cast<unsigned char>(c)) * 16777619u;
        }
        return h;
    }
};

static inline char to_lower_ascii(char c) {
    if (c >= 'A' && c <= 'Z') return static_cast<char>(c + 32);
    return c;
}

static inline bool is_token_char(char c) {
    return (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9');
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

    // 1. Mmap docs.txt
    int d_fd = open(docs_path.c_str(), O_RDONLY);
    if (d_fd < 0) return 1;

    struct stat d_st;
    if (fstat(d_fd, &d_st) < 0 || d_st.st_size == 0) {
        close(d_fd);
        return 1;
    }

    size_t d_size = d_st.st_size;
    const char* d_data = static_cast<const char*>(mmap(nullptr, d_size, PROT_READ, MAP_PRIVATE, d_fd, 0));
    if (d_data == MAP_FAILED) {
        close(d_fd);
        return 1;
    }
    madvise(const_cast<char*>(d_data), d_size, MADV_WILLNEED);

    // Inverted index: term -> sorted vector of doc_ids
    std::unordered_map<std::string, std::vector<uint32_t>> index;
    // Store doc tokens for phrase search
    std::vector<std::vector<std::string>> doc_tokens;
    doc_tokens.reserve(300000);

    const char* p = d_data;
    const char* end = d_data + d_size;

    uint32_t cur_doc_id = 0;
    while (p < end) {
        // Skip doc_id and TAB
        while (p < end && *p != '\t') ++p;
        if (p < end && *p == '\t') ++p;

        const char* text_start = p;
        while (p < end && *p != '\n') ++p;
        const char* text_end = p;
        if (p < end && *p == '\n') ++p;

        std::vector<std::string> tokens;
        const char* tp = text_start;
        while (tp < text_end) {
            char c = to_lower_ascii(*tp);
            if (is_token_char(c)) {
                std::string t;
                while (tp < text_end) {
                    char tc = to_lower_ascii(*tp);
                    if (!is_token_char(tc)) break;
                    t.push_back(tc);
                    ++tp;
                }
                tokens.push_back(std::move(t));
            } else {
                ++tp;
            }
        }

        // Add to inverted index
        std::vector<std::string> unique_tokens = tokens;
        std::sort(unique_tokens.begin(), unique_tokens.end());
        unique_tokens.erase(std::unique(unique_tokens.begin(), unique_tokens.end()), unique_tokens.end());

        for (const auto& term : unique_tokens) {
            index[term].push_back(cur_doc_id);
        }

        doc_tokens.push_back(std::move(tokens));
        cur_doc_id++;
    }
    munmap(const_cast<char*>(d_data), d_size);
    close(d_fd);

    // 2. Read queries.txt
    std::ifstream q_file(queries_path);
    if (!q_file.is_open()) return 1;

    std::vector<std::string> queries;
    queries.reserve(55000);
    std::string q_line;
    while (std::getline(q_file, q_line)) {
        if (!q_line.empty() && q_line.back() == '\r') q_line.pop_back();
        if (!q_line.empty()) {
            queries.push_back(std::move(q_line));
        }
    }
    q_file.close();

    size_t num_queries = queries.size();
    std::vector<std::string> results(num_queries);

    // Helper functions for set operations
    auto op_and = [](const std::vector<uint32_t>& a, const std::vector<uint32_t>& b) {
        std::vector<uint32_t> res;
        res.reserve(std::min(a.size(), b.size()));
        std::set_intersection(a.begin(), a.end(), b.begin(), b.end(), std::back_inserter(res));
        return res;
    };

    auto op_or = [](const std::vector<uint32_t>& a, const std::vector<uint32_t>& b) {
        std::vector<uint32_t> res;
        res.reserve(a.size() + b.size());
        std::set_union(a.begin(), a.end(), b.begin(), b.end(), std::back_inserter(res));
        return res;
    };

    auto op_not = [](const std::vector<uint32_t>& a, const std::vector<uint32_t>& b) {
        std::vector<uint32_t> res;
        res.reserve(a.size());
        std::set_difference(a.begin(), a.end(), b.begin(), b.end(), std::back_inserter(res));
        return res;
    };

    auto format_doc_ids = [](const std::vector<uint32_t>& ids) {
        uint64_t sum = 0;
        for (uint32_t id : ids) sum += id;
        std::string s = std::to_string(ids.size()) + " " + std::to_string(sum);
        size_t limit = std::min(ids.size(), size_t(10));
        for (size_t i = 0; i < limit; ++i) {
            s += " " + std::to_string(ids[i]);
        }
        return s;
    };

    // 3. Parallel query execution across 12 threads
    unsigned int num_threads = 12;
    unsigned int hw = std::thread::hardware_concurrency();
    if (hw > 0 && hw < num_threads) num_threads = hw;
    if (num_queries < num_threads) num_threads = static_cast<unsigned int>(num_queries);
    if (num_threads == 0) num_threads = 1;

    size_t per_worker = (num_queries + num_threads - 1) / num_threads;
    std::vector<std::thread> workers;

    const std::vector<uint32_t> empty_postings;

    for (unsigned int tid = 0; tid < num_threads; ++tid) {
        size_t start_q = tid * per_worker;
        size_t end_q = std::min(num_queries, start_q + per_worker);

        workers.emplace_back([&, start_q, end_q]() {
            for (size_t qi = start_q; qi < end_q; ++qi) {
                const std::string& line = queries[qi];
                if (line.rfind("BOOL ", 0) == 0) {
                    std::vector<std::string> parts;
                    size_t pos = 5;
                    while (pos < line.size()) {
                        size_t next = line.find(' ', pos);
                        if (next == std::string::npos) {
                            parts.push_back(line.substr(pos));
                            break;
                        }
                        parts.push_back(line.substr(pos, next - pos));
                        pos = next + 1;
                    }

                    if (parts.empty()) {
                        results[qi] = "0 0";
                        continue;
                    }

                    auto it = index.find(parts[0]);
                    std::vector<uint32_t> current = (it != index.end()) ? it->second : empty_postings;

                    for (size_t i = 1; i + 1 < parts.size(); i += 2) {
                        const std::string& op = parts[i];
                        const std::string& term = parts[i + 1];
                        auto term_it = index.find(term);
                        const auto& postings = (term_it != index.end()) ? term_it->second : empty_postings;

                        if (op == "AND") {
                            current = op_and(current, postings);
                        } else if (op == "OR") {
                            current = op_or(current, postings);
                        } else if (op == "NOT") {
                            current = op_not(current, postings);
                        }
                    }
                    results[qi] = format_doc_ids(current);
                } else if (line.rfind("PHRASE \"", 0) == 0) {
                    // Extract phrase between quotes
                    size_t start_p = 8;
                    size_t end_p = line.rfind('"');
                    if (end_p == std::string::npos || end_p <= start_p) {
                        results[qi] = "0 0";
                        continue;
                    }
                    std::string phrase = line.substr(start_p, end_p - start_p);
                    std::vector<std::string> pterms;
                    size_t ppos = 0;
                    while (ppos < phrase.size()) {
                        size_t next = phrase.find(' ', ppos);
                        if (next == std::string::npos) {
                            pterms.push_back(phrase.substr(ppos));
                            break;
                        }
                        pterms.push_back(phrase.substr(ppos, next - ppos));
                        ppos = next + 1;
                    }

                    if (pterms.empty()) {
                        results[qi] = "0 0";
                        continue;
                    }

                    auto it = index.find(pterms[0]);
                    std::vector<uint32_t> candidates = (it != index.end()) ? it->second : empty_postings;
                    for (size_t i = 1; i < pterms.size(); ++i) {
                        auto term_it = index.find(pterms[i]);
                        const auto& postings = (term_it != index.end()) ? term_it->second : empty_postings;
                        candidates = op_and(candidates, postings);
                        if (candidates.empty()) break;
                    }

                    size_t k = pterms.size();
                    std::vector<uint32_t> matches;
                    for (uint32_t doc_id : candidates) {
                        const auto& dtoks = doc_tokens[doc_id];
                        if (dtoks.size() < k) continue;
                        bool found = false;
                        for (size_t i = 0; i + k <= dtoks.size(); ++i) {
                            bool ok = true;
                            for (size_t j = 0; j < k; ++j) {
                                if (dtoks[i + j] != pterms[j]) {
                                    ok = false;
                                    break;
                                }
                            }
                            if (ok) {
                                found = true;
                                break;
                            }
                        }
                        if (found) {
                            matches.push_back(doc_id);
                        }
                    }
                    results[qi] = format_doc_ids(matches);
                }
            }
        });
    }

    for (auto& w : workers) {
        w.join();
    }

    // 4. Output results.txt
    int out_fd = open(out_path.c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0666);
    if (out_fd >= 0) {
        std::string buffer;
        buffer.reserve(512 * 1024);
        for (const auto& res : results) {
            buffer.append(res);
            buffer.push_back('\n');
            if (buffer.size() >= 256 * 1024) {
                ssize_t written = write(out_fd, buffer.data(), buffer.size());
                (void)written;
                buffer.clear();
            }
        }
        if (!buffer.empty()) {
            ssize_t written = write(out_fd, buffer.data(), buffer.size());
            (void)written;
        }
        close(out_fd);
    }

    return 0;
}
