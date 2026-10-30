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
#include <unordered_map>
#include <vector>

static inline uint64_t fnv1a_hash(const char* data, size_t len, uint64_t h = 14695981039346656037ULL) {
    for (size_t i = 0; i < len; ++i) {
        h = (h ^ static_cast<uint8_t>(data[i])) * 1099511628211ULL;
    }
    return h;
}

struct LocalGroup {
    const char* ts_ptr = nullptr;
    const char* ep_ptr = nullptr;
    uint16_t ep_len = 0;
    uint32_t count = 0;
    uint32_t errors = 0;
    std::vector<uint32_t> latencies;
};

struct MergedGroup {
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

void parse_chunk(const char* data, size_t start, size_t end, std::unordered_map<uint64_t, LocalGroup>& local_map) {
    const char* p = data + start;
    const char* chunk_end = data + end;

    while (p < chunk_end) {
        if (p + 28 > chunk_end) break;

        const char* ts_start = p;
        p += 28; // past 27 bytes timestamp + space

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

        // Status: 3 digits
        if (p + 3 > chunk_end) break;
        int status = (p[0] - '0') * 100 + (p[1] - '0') * 10 + (p[2] - '0');
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

        // 64-bit hash composite of hour (13 bytes) + endpoint
        uint64_t key_hash = fnv1a_hash(ep_start, ep_len, fnv1a_hash(ts_start, 13));

        auto [it, inserted] = local_map.try_emplace(key_hash);
        if (inserted) {
            it->second.ts_ptr = ts_start;
            it->second.ep_ptr = ep_start;
            it->second.ep_len = static_cast<uint16_t>(ep_len);
            it->second.latencies.reserve(8192);
        }

        LocalGroup& grp = it->second;
        grp.count++;
        if (status >= 500 && status <= 599) {
            grp.errors++;
        }
        grp.latencies.push_back(lat);
    }
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

    std::vector<std::unordered_map<uint64_t, LocalGroup>> local_maps(num_threads);
    for (unsigned int t = 0; t < num_threads; ++t) {
        local_maps[t].reserve(512);
    }

    std::vector<std::thread> workers;
    workers.reserve(num_threads);

    for (unsigned int t = 0; t < num_threads; ++t) {
        workers.emplace_back(parse_chunk, data, splits[t], splits[t + 1], std::ref(local_maps[t]));
    }

    for (auto& w : workers) {
        w.join();
    }

    // Merge into map sorted by (hour, endpoint)
    std::map<std::pair<std::string, std::string>, MergedGroup> merged;
    for (unsigned int t = 0; t < num_threads; ++t) {
        for (auto& [h, grp] : local_maps[t]) {
            std::string hr(grp.ts_ptr, 13);
            std::string ep(grp.ep_ptr, grp.ep_len);

            auto& mg = merged[{hr, ep}];
            if (mg.count == 0) {
                mg.hour = hr;
                mg.endpoint = ep;
                mg.latencies.reserve(grp.count * num_threads);
            }
            mg.count += grp.count;
            mg.errors += grp.errors;
            mg.latencies.insert(mg.latencies.end(), grp.latencies.begin(), grp.latencies.end());
        }
        local_maps[t].clear();
    }

    // Parallel percentile computation
    std::vector<MergedGroup*> group_ptrs;
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
                MergedGroup* mg = group_ptrs[i];
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

    // Write formatted output
    int out_fd = open(out_path.c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0666);
    if (out_fd >= 0) {
        std::string buffer;
        buffer.reserve(2 * 1024 * 1024);

        for (auto* mg : group_ptrs) {
            buffer.append(mg->hour);
            buffer.append(":00:00Z ");
            buffer.append(mg->endpoint);
            buffer.push_back(' ');
            buffer.append(std::to_string(mg->count));
            buffer.push_back(' ');
            buffer.append(std::to_string(mg->p50));
            buffer.push_back(' ');
            buffer.append(std::to_string(mg->p95));
            buffer.push_back(' ');
            buffer.append(std::to_string(mg->p99));
            buffer.push_back(' ');
            buffer.append(std::to_string(mg->errors));
            buffer.push_back(' ');
            buffer.append(std::to_string(mg->error_ppm));
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
