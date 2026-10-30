#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#include <algorithm>
#include <cstdint>
#include <cstring>
#include <map>
#include <string>
#include <string_view>
#include <thread>
#include <vector>

// Direct integer-to-ASCII formatter for maximum I/O throughput
static inline void append_u32(std::string& s, uint32_t v) {
    char buf[12];
    char* p = buf + sizeof(buf);
    if (v == 0) {
        s.push_back('0');
        return;
    }
    while (v > 0) {
        *--p = static_cast<char>('0' + (v % 10));
        v /= 10;
    }
    s.append(p, (buf + sizeof(buf)) - p);
}

static inline void append_u64(std::string& s, uint64_t v) {
    char buf[24];
    char* p = buf + sizeof(buf);
    if (v == 0) {
        s.push_back('0');
        return;
    }
    while (v > 0) {
        *--p = static_cast<char>('0' + (v % 10));
        v /= 10;
    }
    s.append(p, (buf + sizeof(buf)) - p);
}

struct LocalGroup {
    uint32_t count = 0;
    uint32_t errors = 0;
    std::vector<uint32_t> latencies;
};

struct EndpointInfo {
    const char* ptr;
    size_t len;
};

struct ThreadParser {
    std::vector<std::string> hours;
    std::vector<EndpointInfo> endpoints;
    // Map: (hour_id * 64 + ep_id) -> LocalGroup
    std::vector<LocalGroup> groups;

    ThreadParser() {
        hours.reserve(128);
        endpoints.reserve(64);
        groups.resize(128 * 64);
    }
};

void parse_chunk(const char* data, size_t start, size_t end, ThreadParser& parser) {
    const char* p = data + start;
    const char* chunk_end = data + end;

    uint32_t cur_hour_id = UINT32_MAX;
    char last_hour[13] = {0};

    while (p < chunk_end) {
        if (p + 28 > chunk_end) break;

        const char* ts_start = p;
        p += 28; // past 27 bytes timestamp + space

        // Hour caching: hours stay identical for thousands of lines
        if (cur_hour_id == UINT32_MAX || memcmp(ts_start, last_hour, 13) != 0) {
            memcpy(last_hour, ts_start, 13);
            std::string hr_str(ts_start, 13);
            auto it = std::find(parser.hours.begin(), parser.hours.end(), hr_str);
            if (it != parser.hours.end()) {
                cur_hour_id = static_cast<uint32_t>(std::distance(parser.hours.begin(), it));
            } else {
                cur_hour_id = static_cast<uint32_t>(parser.hours.size());
                parser.hours.push_back(std::move(hr_str));
            }
        }

        // Method: scan until space
        while (p < chunk_end && *p != ' ') ++p;
        if (p >= chunk_end) break;
        ++p;

        // Endpoint: scan until space
        const char* ep_start = p;
        while (p < chunk_end && *p != ' ') ++p;
        if (p >= chunk_end) break;
        size_t ep_len = p - ep_start;
        ++p;

        // Endpoint caching (at most ~20 endpoints in the entire corpus)
        uint32_t cur_ep_id = UINT32_MAX;
        for (size_t i = 0; i < parser.endpoints.size(); ++i) {
            if (parser.endpoints[i].len == ep_len && memcmp(parser.endpoints[i].ptr, ep_start, ep_len) == 0) {
                cur_ep_id = static_cast<uint32_t>(i);
                break;
            }
        }
        if (cur_ep_id == UINT32_MAX) {
            cur_ep_id = static_cast<uint32_t>(parser.endpoints.size());
            parser.endpoints.push_back({ep_start, ep_len});
        }

        // Status code: 3 ASCII digits
        if (p + 3 > chunk_end) break;
        bool is_error = (p[0] == '5');
        p += 3;
        if (p < chunk_end && *p == ' ') ++p;

        // Latency in microseconds
        uint32_t lat = 0;
        while (p < chunk_end && *p >= '0' && *p <= '9') {
            lat = lat * 10 + (*p - '0');
            ++p;
        }
        if (p < chunk_end && *p == '\r') ++p;
        if (p < chunk_end && *p == '\n') ++p;

        // Direct flat array indexing: Zero hash map, zero collision, L1 cache hit!
        size_t group_idx = static_cast<size_t>(cur_hour_id) * 64 + cur_ep_id;
        if (group_idx >= parser.groups.size()) {
            parser.groups.resize((cur_hour_id + 1) * 64);
        }

        LocalGroup& grp = parser.groups[group_idx];
        if (grp.count == 0) {
            grp.latencies.reserve(8192);
        }
        grp.count++;
        if (is_error) grp.errors++;
        grp.latencies.push_back(lat);
    }
}

struct OutputGroup {
    std::string hour;
    std::string endpoint;
    uint64_t count = 0;
    uint64_t errors = 0;
    std::vector<uint32_t> latencies;
    uint32_t p50 = 0;
    uint32_t p95 = 0;
    uint32_t p99 = 0;
    uint64_t error_ppm = 0;
};

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

    std::string in_path = data_dir + "/requests.log";
    std::string out_path = out_dir + "/summary.txt";

    int fd = open(in_path.c_str(), O_RDONLY);
    if (fd < 0) {
        int out_fd = open(out_path.c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0666);
        if (out_fd >= 0) close(out_fd);
        return 0;
    }

    struct stat st;
    if (fstat(fd, &st) < 0 || st.st_size == 0) {
        close(fd);
        int out_fd = open(out_path.c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0666);
        if (out_fd >= 0) close(out_fd);
        return 0;
    }

    size_t file_size = st.st_size;
    const char* data = static_cast<const char*>(mmap(nullptr, file_size, PROT_READ, MAP_PRIVATE, fd, 0));
    if (data == MAP_FAILED) {
        close(fd);
        return 1;
    }
    madvise(const_cast<char*>(data), file_size, MADV_WILLNEED);

    unsigned int num_threads = 12;
    unsigned int hw = std::thread::hardware_concurrency();
    if (hw > 0 && hw < num_threads) num_threads = hw;
    if (file_size < 1000000) num_threads = 1;

    std::vector<size_t> splits(num_threads + 1);
    splits[0] = 0;
    splits[num_threads] = file_size;
    for (unsigned int t = 1; t < num_threads; ++t) {
        size_t pt = t * (file_size / num_threads);
        while (pt < file_size && data[pt] != '\n') ++pt;
        if (pt < file_size) ++pt;
        splits[t] = pt;
    }

    std::vector<ThreadParser> parsers(num_threads);
    std::vector<std::thread> workers;
    workers.reserve(num_threads);

    for (unsigned int t = 0; t < num_threads; ++t) {
        workers.emplace_back(parse_chunk, data, splits[t], splits[t + 1], std::ref(parsers[t]));
    }

    for (auto& w : workers) {
        w.join();
    }

    // Merge into map sorted by (hour, endpoint)
    std::map<std::pair<std::string, std::string>, OutputGroup> merged;

    for (unsigned int t = 0; t < num_threads; ++t) {
        const auto& p = parsers[t];
        for (size_t hid = 0; hid < p.hours.size(); ++hid) {
            for (size_t eid = 0; eid < p.endpoints.size(); ++eid) {
                size_t gidx = hid * 64 + eid;
                if (gidx < p.groups.size() && p.groups[gidx].count > 0) {
                    const auto& grp = p.groups[gidx];
                    std::string ep_str(p.endpoints[eid].ptr, p.endpoints[eid].len);
                    auto& mg = merged[{p.hours[hid], ep_str}];
                    if (mg.count == 0) {
                        mg.hour = p.hours[hid];
                        mg.endpoint = std::move(ep_str);
                        mg.latencies.reserve(grp.count * num_threads);
                    }
                    mg.count += grp.count;
                    mg.errors += grp.errors;
                    mg.latencies.insert(mg.latencies.end(), grp.latencies.begin(), grp.latencies.end());
                }
            }
        }
    }

    // Parallel percentile computation
    std::vector<OutputGroup*> group_ptrs;
    group_ptrs.reserve(merged.size());
    for (auto& [pair, mg] : merged) {
        group_ptrs.push_back(&mg);
    }

    size_t total_groups = group_ptrs.size();
    unsigned int sort_threads = std::min(num_threads, static_cast<unsigned int>(total_groups));
    if (sort_threads == 0) sort_threads = 1;
    size_t per_worker = (total_groups + sort_threads - 1) / sort_threads;

    std::vector<std::thread> sort_workers;
    for (unsigned int t = 0; t < sort_threads; ++t) {
        size_t start_idx = t * per_worker;
        size_t end_idx = std::min(total_groups, start_idx + per_worker);
        sort_workers.emplace_back([&group_ptrs, start_idx, end_idx]() {
            for (size_t i = start_idx; i < end_idx; ++i) {
                OutputGroup* mg = group_ptrs[i];
                std::sort(mg->latencies.begin(), mg->latencies.end());
                uint64_t n = mg->count;
                uint64_t r50 = (50ULL * n + 99) / 100;
                uint64_t r95 = (95ULL * n + 99) / 100;
                uint64_t r99 = (99ULL * n + 99) / 100;
                mg->p50 = mg->latencies[r50 - 1];
                mg->p95 = mg->latencies[r95 - 1];
                mg->p99 = mg->latencies[r99 - 1];
                mg->error_ppm = (static_cast<uint64_t>(mg->errors) * 1000000ULL) / n;
            }
        });
    }

    for (auto& sw : sort_workers) {
        sw.join();
    }

    // High-speed buffered I/O write
    int out_fd = open(out_path.c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0666);
    if (out_fd >= 0) {
        std::string buffer;
        buffer.reserve(2 * 1024 * 1024);

        for (auto* mg : group_ptrs) {
            buffer.append(mg->hour);
            buffer.append(":00:00Z ");
            buffer.append(mg->endpoint);
            buffer.push_back(' ');
            append_u64(buffer, mg->count);
            buffer.push_back(' ');
            append_u32(buffer, mg->p50);
            buffer.push_back(' ');
            append_u32(buffer, mg->p95);
            buffer.push_back(' ');
            append_u32(buffer, mg->p99);
            buffer.push_back(' ');
            append_u64(buffer, mg->errors);
            buffer.push_back(' ');
            append_u64(buffer, mg->error_ppm);
            buffer.push_back('\n');
        }

        if (!buffer.empty()) {
            ssize_t written = write(out_fd, buffer.data(), buffer.size());
            (void)written;
        }
        close(out_fd);
    }

    munmap(const_cast<char*>(data), file_size);
    close(fd);
    return 0;
}
