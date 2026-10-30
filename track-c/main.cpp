#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#include <algorithm>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <string_view>
#include <thread>
#include <unordered_map>
#include <vector>

struct CountryAgg {
    std::string key;
    uint16_t cid = 0;
    uint64_t count = 0;
    uint64_t id_sum = 0;
    int64_t amount_sum = 0;
    int64_t amount_min = 0;
    int64_t amount_max = 0;
    uint64_t name_bytes = 0;
    uint64_t note_bytes = 0;
    size_t distinct_names = 0;
};

struct LocalCountryAgg {
    uint64_t count = 0;
    uint64_t id_sum = 0;
    int64_t amount_sum = 0;
    int64_t amount_min = INT64_MAX;
    int64_t amount_max = INT64_MIN;
    uint64_t name_bytes = 0;
    uint64_t note_bytes = 0;
    std::vector<uint64_t> name_hashes;
};

static inline void format_cents(int64_t cents, std::string& out) {
    if (cents < 0) {
        out.push_back('-');
        cents = -cents;
    }
    out.append(std::to_string(cents / 100));
    out.push_back('.');
    int rem = static_cast<int>(cents % 100);
    out.push_back(static_cast<char>('0' + (rem / 10)));
    out.push_back(static_cast<char>('0' + (rem % 10)));
}

static uint16_t region_to_country[10000000];

void parse_csv_chunk(const char* rec_data, size_t start, size_t end, size_t num_countries, std::vector<LocalCountryAgg>& local_aggs) {
    const char* p = rec_data + start;
    const char* chunk_end = rec_data + end;

    while (p < chunk_end) {
        // Check for mid-file BOM
        if (p + 3 <= chunk_end && static_cast<uint8_t>(p[0]) == 0xEF &&
            static_cast<uint8_t>(p[1]) == 0xBB && static_cast<uint8_t>(p[2]) == 0xBF) {
            p += 3;
        }
        if (p >= chunk_end) break;

        // Field 0: id
        uint64_t record_id = 0;
        while (p < chunk_end && *p >= '0' && *p <= '9') {
            record_id = record_id * 10 + (*p - '0');
            ++p;
        }
        if (p < chunk_end && *p == ',') ++p;

        // Field 1: name (FNV-1a 64-bit zero-allocation hash & length)
        uint64_t name_hash = 14695981039346656037ULL;
        uint64_t name_len = 0;
        if (p < chunk_end && *p == '"') {
            ++p;
            while (p < chunk_end) {
                if (*p == '"') {
                    if (p + 1 < chunk_end && *(p + 1) == '"') {
                        name_hash = (name_hash ^ static_cast<uint8_t>('"')) * 1099511628211ULL;
                        name_len += 1;
                        p += 2;
                    } else {
                        ++p;
                        break;
                    }
                } else {
                    name_hash = (name_hash ^ static_cast<uint8_t>(*p)) * 1099511628211ULL;
                    name_len += 1;
                    ++p;
                }
            }
        } else {
            const char* nstart = p;
            while (p < chunk_end && *p != ',') {
                name_hash = (name_hash ^ static_cast<uint8_t>(*p)) * 1099511628211ULL;
                ++p;
            }
            name_len = p - nstart;
        }
        if (p < chunk_end && *p == ',') ++p;

        // Field 2: note (length only)
        uint64_t note_len = 0;
        if (p < chunk_end && *p == '"') {
            ++p;
            while (p < chunk_end) {
                if (*p == '"') {
                    if (p + 1 < chunk_end && *(p + 1) == '"') {
                        note_len += 1;
                        p += 2;
                    } else {
                        ++p;
                        break;
                    }
                } else {
                    note_len += 1;
                    ++p;
                }
            }
        } else {
            const char* ntstart = p;
            while (p < chunk_end && *p != ',') ++p;
            note_len = p - ntstart;
        }
        if (p < chunk_end && *p == ',') ++p;

        // Field 3: region_id
        uint32_t region_id = 0;
        while (p < chunk_end && *p >= '0' && *p <= '9') {
            region_id = region_id * 10 + (*p - '0');
            ++p;
        }
        if (p < chunk_end && *p == ',') ++p;

        // Field 4: amount
        bool neg = false;
        if (p < chunk_end && *p == '-') {
            neg = true;
            ++p;
        }
        int64_t whole = 0;
        while (p < chunk_end && *p >= '0' && *p <= '9') {
            whole = whole * 10 + (*p - '0');
            ++p;
        }
        if (p < chunk_end && *p == '.') ++p;
        int64_t frac = 0;
        if (p < chunk_end && *p >= '0' && *p <= '9') {
            frac = frac * 10 + (*p - '0');
            ++p;
        }
        if (p < chunk_end && *p >= '0' && *p <= '9') {
            frac = frac * 10 + (*p - '0');
            ++p;
        }
        int64_t cents = whole * 100 + frac;
        if (neg) cents = -cents;

        while (p < chunk_end && (*p == '\r' || *p == '\n')) ++p;

        uint16_t cid = (region_id < 10000000) ? region_to_country[region_id] : 0;
        LocalCountryAgg& agg = local_aggs[cid];
        agg.count++;
        agg.id_sum += record_id;
        agg.amount_sum += cents;
        if (cents < agg.amount_min) agg.amount_min = cents;
        if (cents > agg.amount_max) agg.amount_max = cents;
        agg.name_bytes += name_len;
        agg.note_bytes += note_len;
        agg.name_hashes.push_back(name_hash);
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

    // 1. Load regions.csv
    std::string regions_path = data_dir + "/regions.csv";
    int r_fd = open(regions_path.c_str(), O_RDONLY);
    if (r_fd < 0) return 1;

    struct stat r_st;
    if (fstat(r_fd, &r_st) < 0) {
        close(r_fd);
        return 1;
    }
    size_t r_size = r_st.st_size;
    const char* r_data = static_cast<const char*>(mmap(nullptr, r_size, PROT_READ, MAP_PRIVATE, r_fd, 0));
    madvise(const_cast<char*>(r_data), r_size, MADV_WILLNEED);

    std::vector<CountryAgg> countries;
    countries.reserve(300);

    CountryAgg unmatched_agg;
    unmatched_agg.key = "_UNMATCHED";
    unmatched_agg.cid = 0;
    countries.push_back(std::move(unmatched_agg));

    std::unordered_map<std::string, uint16_t> country_map;
    country_map.reserve(300);

    const char* rp = r_data;
    const char* rend = r_data + r_size;

    while (rp < rend && *rp != '\n') ++rp;
    if (rp < rend && *rp == '\n') ++rp;

    while (rp < rend) {
        if (*rp == '\r' || *rp == '\n') {
            ++rp;
            continue;
        }
        uint32_t region_id = 0;
        while (rp < rend && *rp >= '0' && *rp <= '9') {
            region_id = region_id * 10 + (*rp - '0');
            ++rp;
        }
        if (rp < rend && *rp == ',') ++rp;
        const char* c_start = rp;
        while (rp < rend && *rp != '\r' && *rp != '\n') ++rp;
        std::string country_name(c_start, rp - c_start);
        while (rp < rend && (*rp == '\r' || *rp == '\n')) ++rp;

        auto [it, inserted] = country_map.try_emplace(country_name, static_cast<uint16_t>(countries.size()));
        if (inserted) {
            CountryAgg agg;
            agg.key = country_name;
            agg.cid = it->second;
            countries.push_back(std::move(agg));
        }

        if (region_id < 10000000) {
            region_to_country[region_id] = it->second;
        }
    }

    munmap(const_cast<char*>(r_data), r_size);
    close(r_fd);

    size_t num_countries = countries.size();

    // 2. Load records.csv
    std::string records_path = data_dir + "/records.csv";
    int rec_fd = open(records_path.c_str(), O_RDONLY);
    if (rec_fd < 0) return 1;

    struct stat rec_st;
    if (fstat(rec_fd, &rec_st) < 0) {
        close(rec_fd);
        return 1;
    }
    size_t rec_size = rec_st.st_size;
    const char* rec_data = static_cast<const char*>(mmap(nullptr, rec_size, PROT_READ, MAP_PRIVATE, rec_fd, 0));
    madvise(const_cast<char*>(rec_data), rec_size, MADV_WILLNEED);

    const char* p = rec_data;
    const char* end = rec_data + rec_size;

    if (p + 3 <= end && static_cast<uint8_t>(p[0]) == 0xEF &&
        static_cast<uint8_t>(p[1]) == 0xBB && static_cast<uint8_t>(p[2]) == 0xBF) {
        p += 3;
    }

    while (p < end && *p != '\n') ++p;
    if (p < end && *p == '\n') ++p;

    size_t data_start = p - rec_data;

    // Multi-threaded 12-thread parallel CSV parsing
    unsigned int num_threads = 12;
    unsigned int hw = std::thread::hardware_concurrency();
    if (hw > 0 && hw < num_threads) num_threads = hw;
    if (rec_size < 1000000) num_threads = 1;

    std::vector<size_t> splits(num_threads + 1);
    splits[0] = data_start;
    splits[num_threads] = rec_size;

    for (unsigned int t = 1; t < num_threads; ++t) {
        size_t pt = data_start + t * ((rec_size - data_start) / num_threads);
        while (pt < rec_size) {
            if (rec_data[pt] == '\n') {
                if (pt >= 3 && rec_data[pt - 3] == '.') {
                    ++pt;
                    break;
                }
            }
            ++pt;
        }
        splits[t] = std::min(pt, rec_size);
    }

    std::vector<std::vector<LocalCountryAgg>> thread_aggs(num_threads, std::vector<LocalCountryAgg>(num_countries));
    for (unsigned int t = 0; t < num_threads; ++t) {
        for (size_t c = 0; c < num_countries; ++c) {
            thread_aggs[t][c].name_hashes.reserve(4096);
        }
    }

    std::vector<std::thread> workers;
    for (unsigned int t = 0; t < num_threads; ++t) {
        workers.emplace_back(parse_csv_chunk, rec_data, splits[t], splits[t + 1], num_countries, std::ref(thread_aggs[t]));
    }

    for (auto& w : workers) {
        w.join();
    }

    munmap(const_cast<char*>(rec_data), rec_size);
    close(rec_fd);

    // Merge thread aggregates
    std::vector<std::vector<uint64_t>> country_name_hashes(num_countries);
    for (size_t c = 0; c < num_countries; ++c) {
        CountryAgg& dest = countries[c];
        bool has_records = false;
        for (unsigned int t = 0; t < num_threads; ++t) {
            const LocalCountryAgg& src = thread_aggs[t][c];
            if (src.count > 0) {
                has_records = true;
                dest.count += src.count;
                dest.id_sum += src.id_sum;
                dest.amount_sum += src.amount_sum;
                if (src.amount_min < dest.amount_min || dest.count == src.count) dest.amount_min = src.amount_min;
                if (src.amount_max > dest.amount_max || dest.count == src.count) dest.amount_max = src.amount_max;
                dest.name_bytes += src.name_bytes;
                dest.note_bytes += src.note_bytes;
                country_name_hashes[c].insert(country_name_hashes[c].end(), src.name_hashes.begin(), src.name_hashes.end());
            }
        }
    }

    // Parallel deduplication of distinct names
    std::vector<std::thread> sort_workers;
    for (unsigned int t = 0; t < num_threads; ++t) {
        size_t start_c = t * ((num_countries + num_threads - 1) / num_threads);
        size_t end_c = std::min(num_countries, start_c + ((num_countries + num_threads - 1) / num_threads));
        sort_workers.emplace_back([&country_name_hashes, &countries, start_c, end_c]() {
            for (size_t i = start_c; i < end_c; ++i) {
                if (countries[i].count > 0) {
                    std::sort(country_name_hashes[i].begin(), country_name_hashes[i].end());
                    country_name_hashes[i].erase(std::unique(country_name_hashes[i].begin(), country_name_hashes[i].end()), country_name_hashes[i].end());
                    countries[i].distinct_names = country_name_hashes[i].size();
                }
            }
        });
    }

    for (auto& sw : sort_workers) {
        sw.join();
    }

    // Filter to countries with count > 0 and sort by key
    std::vector<CountryAgg*> active_countries;
    for (size_t i = 0; i < num_countries; ++i) {
        if (countries[i].count > 0) {
            active_countries.push_back(&countries[i]);
        }
    }

    std::sort(active_countries.begin(), active_countries.end(), [](const CountryAgg* a, const CountryAgg* b) {
        return a->key < b->key;
    });

    // 4. Output aggregates.txt
    std::string out_path = out_dir + "/aggregates.txt";
    int out_fd = open(out_path.c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0666);
    if (out_fd >= 0) {
        std::string buffer;
        buffer.reserve(64 * 1024);

        for (const auto* c : active_countries) {
            buffer.append(c->key);
            buffer.push_back(' ');
            buffer.append(std::to_string(c->count));
            buffer.push_back(' ');
            buffer.append(std::to_string(c->id_sum));
            buffer.push_back(' ');
            format_cents(c->amount_sum, buffer);
            buffer.push_back(' ');
            format_cents(c->amount_min, buffer);
            buffer.push_back(' ');
            format_cents(c->amount_max, buffer);
            buffer.push_back(' ');
            buffer.append(std::to_string(c->distinct_names));
            buffer.push_back(' ');
            buffer.append(std::to_string(c->name_bytes));
            buffer.push_back(' ');
            buffer.append(std::to_string(c->note_bytes));
            buffer.push_back('\n');
        }

        if (!buffer.empty()) {
            ssize_t written = write(out_fd, buffer.data(), buffer.size());
            (void)written;
        }
        close(out_fd);
    }

    return 0;
}
