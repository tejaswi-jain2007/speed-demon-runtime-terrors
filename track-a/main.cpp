#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#include <algorithm>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <map>
#include <string>
#include <string_view>
#include <thread>
#include <vector>

static inline uint64_t fnv1a(const char* data, size_t len, uint64_t h = 14695981039346656037ULL) {
    for (size_t i = 0; i < len; ++i) {
        h = (h ^ static_cast<uint8_t>(data[i])) * 1099511628211ULL;
    }
    return h;
}

struct FastGroup {
    uint64_t hash = 0;
    uint32_t ts_offset = 0;
    uint32_t ep_offset = 0;
    uint16_t ep_len = 0;
    uint32_t count = 0;
    uint32_t errors = 0;
    std::vector<uint32_t> latencies;
};

// 16384 slots table: ~3% load factor for 500 groups -> 1 probe average in L1
constexpr size_t TABLE_CAP = 16384;
constexpr size_t TABLE_MASK = TABLE_CAP - 1;

void parse_chunk(const char* data, size_t start, size_t end, std::vector<FastGroup>& table, std::vector<uint16_t>& active_slots) {
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

        // 64-bit FNV-1a hash of (hour 13 bytes, endpoint)
        uint64_t h = fnv1a(ep_start, ep_len, fnv1a(ts_start, 13));

        size_t slot = static_cast<size_t>(h) & TABLE_MASK;
        while (table[slot].hash != 0) {
            if (table[slot].hash == h &&
                table[slot].ep_len == ep_len &&
                memcmp(data + table[slot].ts_offset, ts_start, 13) == 0 &&
                memcmp(data + table[slot].ep_offset, ep_start, ep_len) == 0) {
                break;
            }
            slot = (slot + 1) & TABLE_MASK;
        }

        FastGroup& grp = table[slot];
        if (grp.hash == 0) {
            grp.hash = h;
            grp.ts_offset = static_cast<uint32_t>(ts_start - data);
            grp.ep_offset = static_cast<uint32_t>(ep_start - data);
            grp.ep_len = static_cast<uint16_t>(ep_len);
            grp.latencies.reserve(16384);
            active_slots.push_back(static_cast<uint16_t>(slot));
        }

        grp.count++;
        if (status >= 500 && status <= 599) {
            grp.errors++;
        }
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

    std::vector<std::vector<FastGroup>> thread_tables(num_threads, std::vector<FastGroup>(TABLE_CAP));
    std::vector<std::vector<uint16_t>> thread_active(num_threads);
    std::vector<std::thread> workers;
    workers.reserve(num_threads);

    for (unsigned int t = 0; t < num_threads; ++t) {
        workers.emplace_back(parse_chunk, data, splits[t], splits[t + 1],
                             std::ref(thread_tables[t]), std::ref(thread_active[t]));
    }

    for (auto& w : workers) {
        w.join();
    }

    // Merge active slots into canonical ordered map
    std::map<std::pair<std::string, std::string>, OutputGroup> merged;

    for (unsigned int t = 0; t < num_threads; ++t) {
        for (uint16_t slot : thread_active[t]) {
            FastGroup& grp = thread_tables[t][slot];
            std::string hr(data + grp.ts_offset, 13);
            std::string ep(data + grp.ep_offset, grp.ep_len);

            auto& out_grp = merged[{hr, ep}];
            if (out_grp.count == 0) {
                out_grp.hour = hr;
                out_grp.endpoint = ep;
                out_grp.latencies.reserve(grp.count * num_threads);
            }
            out_grp.count += grp.count;
            out_grp.errors += grp.errors;
            out_grp.latencies.insert(out_grp.latencies.end(), grp.latencies.begin(), grp.latencies.end());
        }
    }

    // Parallel percentile computation
    std::vector<OutputGroup*> group_ptrs;
    group_ptrs.reserve(merged.size());
    for (auto& [key, grp] : merged) {
        group_ptrs.push_back(&grp);
    }

    size_t total_groups = group_ptrs.size();
    unsigned int sort_threads = std::min(num_threads, static_cast<unsigned int>(total_groups));
    if (sort_threads == 0) sort_threads = 1;
    size_t per_worker = (total_groups + sort_threads - 1) / sort_threads;

    std::vector<std::thread> sort_workers;
    for (unsigned int t = 0; t < sort_threads; ++t) {
        size_t start_i = t * per_worker;
        size_t end_i = std::min(total_groups, start_i + per_worker);
        sort_workers.emplace_back([&group_ptrs, start_i, end_i]() {
            for (size_t i = start_i; i < end_i; ++i) {
                OutputGroup* grp = group_ptrs[i];
                std::sort(grp->latencies.begin(), grp->latencies.end());
                uint64_t n = grp->count;
                uint64_t r50 = (50ULL * n + 99) / 100;
                uint64_t r95 = (95ULL * n + 99) / 100;
                uint64_t r99 = (99ULL * n + 99) / 100;
                grp->p50 = grp->latencies[r50 - 1];
                grp->p95 = grp->latencies[r95 - 1];
                grp->p99 = grp->latencies[r99 - 1];
                grp->error_ppm = (static_cast<uint64_t>(grp->errors) * 1000000ULL) / n;
            }
        });
    }

    for (auto& sw : sort_workers) {
        sw.join();
    }

    // Format output
    int out_fd = open(out_path.c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0666);
    if (out_fd >= 0) {
        std::string buffer;
        buffer.reserve(2 * 1024 * 1024);

        for (OutputGroup* grp : group_ptrs) {
            // Format: YYYY-MM-DDTHH:00:00Z endpoint count p50 p95 p99 errors error_ppm\n
            buffer.append(grp->hour);
            buffer.append(":00:00Z ");
            buffer.append(grp->endpoint);
            buffer.push_back(' ');
            buffer.append(std::to_string(grp->count));
            buffer.push_back(' ');
            buffer.append(std::to_string(grp->p50));
            buffer.push_back(' ');
            buffer.append(std::to_string(grp->p95));
            buffer.push_back(' ');
            buffer.append(std::to_string(grp->p99));
            buffer.push_back(' ');
            buffer.append(std::to_string(grp->errors));
            buffer.push_back(' ');
            buffer.append(std::to_string(grp->error_ppm));
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
