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
};

struct RegionLookup {
    std::vector<uint16_t> flat;
    std::vector<std::pair<uint32_t, uint16_t>> ht;
    uint32_t ht_mask = 0;

    inline uint16_t get(uint32_t rid) const {
        if (!flat.empty()) {
            return rid < flat.size() ? flat[rid] : 0;
        }
        if (ht_mask == 0) return 0;
        uint32_t idx = (rid * 2654435761U) & ht_mask;
        while (ht[idx].second != 0) {
            if (ht[idx].first == rid) return ht[idx].second;
            idx = (idx + 1) & ht_mask;
        }
        return 0;
    }
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

void parse_csv_chunk(const char* rec_data, size_t start, size_t end, const RegionLookup& rlookup,
                     std::vector<LocalCountryAgg>& local_aggs, std::vector<std::pair<uint16_t, uint64_t>>& local_names) {
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

        // Field 1: name (FNV-1a 64-bit zero-allocation hash & unquoted length)
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

        // Field 2: note (unquoted length)
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

        uint16_t cid = rlookup.get(region_id);
        LocalCountryAgg& agg = local_aggs[cid];
        agg.count++;
        agg.id_sum += record_id;
        agg.amount_sum += cents;
        if (cents < agg.amount_min) agg.amount_min = cents;
        if (cents > agg.amount_max) agg.amount_max = cents;
        agg.name_bytes += name_len;
        agg.note_bytes += note_len;
        local_names.emplace_back(cid, name_hash);
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

    std::string rec_path = data_dir + "/records.csv";
    std::string reg_path = data_dir + "/regions.csv";
    std::string out_path = out_dir + "/aggregates.txt";

    // 1. Load regions.csv with zero bloated BSS array
    int r_fd = open(reg_path.c_str(), O_RDONLY);
    if (r_fd < 0) return 1;

    struct stat r_st;
    if (fstat(r_fd, &r_st) < 0 || r_st.st_size == 0) {
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

    struct RawRegion {
        uint32_t rid;
        uint16_t cid;
    };
    std::vector<RawRegion> raw_regions;
    raw_regions.reserve(100000);

    const char* rp = r_data;
    const char* rend = r_data + r_size;

    while (rp < rend && *rp != '\n') ++rp;
    if (rp < rend && *rp == '\n') ++rp;

    uint32_t max_rid = 0;
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

        if (region_id > max_rid) max_rid = region_id;
        raw_regions.push_back({region_id, it->second});
    }

    munmap(const_cast<char*>(r_data), r_size);
    close(r_fd);

    RegionLookup rlookup;
    if (max_rid < 1000000) {
        rlookup.flat.assign(max_rid + 1, 0);
        for (const auto& rr : raw_regions) {
            rlookup.flat[rr.rid] = rr.cid;
        }
    } else {
        uint32_t capacity = 16;
        while (capacity < raw_regions.size() * 2) capacity *= 2;
        rlookup.ht_mask = capacity - 1;
        rlookup.ht.assign(capacity, {0, 0});
        for (const auto& rr : raw_regions) {
            uint32_t idx = (rr.rid * 2654435761U) & rlookup.ht_mask;
            while (rlookup.ht[idx].second != 0) {
                idx = (idx + 1) & rlookup.ht_mask;
            }
            rlookup.ht[idx] = {rr.rid, rr.cid};
        }
    }
    raw_regions.clear();
    raw_regions.shrink_to_fit();

    // 2. Load records.csv
    int rec_fd = open(rec_path.c_str(), O_RDONLY);
    if (rec_fd < 0) return 1;

    struct stat rec_st;
    if (fstat(rec_fd, &rec_st) < 0 || rec_st.st_size == 0) {
        close(rec_fd);
        return 1;
    }

    size_t rec_size = rec_st.st_size;
    const char* rec_data = static_cast<const char*>(mmap(nullptr, rec_size, PROT_READ, MAP_PRIVATE, rec_fd, 0));
    madvise(const_cast<char*>(rec_data), rec_size, MADV_WILLNEED);

    const char* hdr_p = rec_data;
    if (hdr_p + 3 <= rec_data + rec_size && static_cast<uint8_t>(hdr_p[0]) == 0xEF &&
        static_cast<uint8_t>(hdr_p[1]) == 0xBB && static_cast<uint8_t>(hdr_p[2]) == 0xBF) {
        hdr_p += 3;
    }
    while (hdr_p < rec_data + rec_size && *hdr_p != '\n') ++hdr_p;
    if (hdr_p < rec_data + rec_size && *hdr_p == '\n') ++hdr_p;
    size_t data_start = hdr_p - rec_data;

    unsigned int num_threads = 12;
    unsigned int hw = std::thread::hardware_concurrency();
    if (hw > 0 && hw < num_threads) num_threads = hw;
    if (rec_size < 1000000) num_threads = 1;

    size_t payload_len = rec_size - data_start;
    std::vector<size_t> splits(num_threads + 1);
    splits[0] = data_start;
    splits[num_threads] = rec_size;

    size_t num_countries = countries.size();

    // True-terminator record detection across CSV chunks
    for (unsigned int t = 1; t < num_threads; ++t) {
        size_t pt = data_start + t * (payload_len / num_threads);
        while (pt < rec_size) {
            if (rec_data[pt] == '\n') {
                size_t probe = pt + 1;
                if (probe + 3 <= rec_size && static_cast<uint8_t>(rec_data[probe]) == 0xEF &&
                    static_cast<uint8_t>(rec_data[probe + 1]) == 0xBB && static_cast<uint8_t>(rec_data[probe + 2]) == 0xBF) {
                    probe += 3;
                }
                const char* endp = rec_data + rec_size;
                const char* cur = rec_data + probe;
                int commas = 0;
                bool valid = true;
                while (cur < endp && *cur != '\n' && *cur != '\r') {
                    if (*cur == '"') {
                        ++cur;
                        while (cur < endp) {
                            if (*cur == '"') {
                                if (cur + 1 < endp && *(cur + 1) == '"') cur += 2;
                                else { ++cur; break; }
                            } else ++cur;
                        }
                    } else {
                        if (*cur == ',') ++commas;
                        ++cur;
                    }
                }
                if (commas == 4) {
                    pt = probe;
                    break;
                }
            }
            ++pt;
        }
        splits[t] = std::min(pt, rec_size);
    }

    std::vector<std::vector<LocalCountryAgg>> thread_aggs(num_threads, std::vector<LocalCountryAgg>(num_countries));
    std::vector<std::vector<std::pair<uint16_t, uint64_t>>> thread_names(num_threads);

    std::vector<std::thread> workers;
    for (unsigned int t = 0; t < num_threads; ++t) {
        workers.emplace_back(parse_csv_chunk, rec_data, splits[t], splits[t + 1], std::cref(rlookup),
                             std::ref(thread_aggs[t]), std::ref(thread_names[t]));
    }

    for (auto& w : workers) {
        w.join();
    }

    munmap(const_cast<char*>(rec_data), rec_size);
    close(rec_fd);

    // Merge basic metrics
    for (size_t c = 0; c < num_countries; ++c) {
        CountryAgg& dest = countries[c];
        for (unsigned int t = 0; t < num_threads; ++t) {
            const LocalCountryAgg& src = thread_aggs[t][c];
            if (src.count > 0) {
                dest.count += src.count;
                dest.id_sum += src.id_sum;
                dest.amount_sum += src.amount_sum;
                if (src.amount_min < dest.amount_min || dest.count == src.count) dest.amount_min = src.amount_min;
                if (src.amount_max > dest.amount_max || dest.count == src.count) dest.amount_max = src.amount_max;
                dest.name_bytes += src.name_bytes;
                dest.note_bytes += src.note_bytes;
            }
        }
    }
    thread_aggs.clear();
    thread_aggs.shrink_to_fit();

    // Group names by country
    std::vector<std::vector<uint64_t>> country_names(num_countries);
    for (unsigned int t = 0; t < num_threads; ++t) {
        for (const auto& item : thread_names[t]) {
            country_names[item.first].push_back(item.second);
        }
        thread_names[t].clear();
        thread_names[t].shrink_to_fit();
    }
    thread_names.clear();
    thread_names.shrink_to_fit();

    // Parallel deduplication of distinct names per country
    std::vector<std::thread> sort_workers;
    for (unsigned int t = 0; t < num_threads; ++t) {
        size_t start_c = t * ((num_countries + num_threads - 1) / num_threads);
        size_t end_c = std::min(num_countries, start_c + ((num_countries + num_threads - 1) / num_threads));
        sort_workers.emplace_back([&country_names, &countries, start_c, end_c]() {
            for (size_t c = start_c; c < end_c; ++c) {
                auto& names = country_names[c];
                if (names.empty()) {
                    countries[c].distinct_names = 0;
                } else {
                    std::sort(names.begin(), names.end());
                    names.erase(std::unique(names.begin(), names.end()), names.end());
                    countries[c].distinct_names = names.size();
                }
                names.clear();
                names.shrink_to_fit();
            }
        });
    }

    for (auto& sw : sort_workers) {
        sw.join();
    }

    // Sort countries lexicographically by country name (UTF-8 byte order)
    std::sort(countries.begin(), countries.end(), [](const CountryAgg& a, const CountryAgg& b) {
        return a.key < b.key;
    });

    // Write output aggregates.txt
    int out_fd = open(out_path.c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0666);
    if (out_fd >= 0) {
        std::string buffer;
        buffer.reserve(128 * 1024);

        for (const auto& c : countries) {
            if (c.count == 0) continue;
            buffer.append(c.key);
            buffer.push_back(' ');
            buffer.append(std::to_string(c.count));
            buffer.push_back(' ');
            buffer.append(std::to_string(c.id_sum));
            buffer.push_back(' ');
            format_cents(c.amount_sum, buffer);
            buffer.push_back(' ');
            format_cents(c.amount_min, buffer);
            buffer.push_back(' ');
            format_cents(c.amount_max, buffer);
            buffer.push_back(' ');
            buffer.append(std::to_string(c.distinct_names));
            buffer.push_back(' ');
            buffer.append(std::to_string(c.name_bytes));
            buffer.push_back(' ');
            buffer.append(std::to_string(c.note_bytes));
            buffer.push_back('\n');
        }

        ssize_t written = write(out_fd, buffer.data(), buffer.size());
        (void)written;
        close(out_fd);
    }

    return 0;
}
