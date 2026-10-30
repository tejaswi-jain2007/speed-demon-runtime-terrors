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

// BSS page-demand allocated zero table
static uint16_t region_to_country[10000000];

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
    if (r_data == MAP_FAILED) {
        close(r_fd);
        return 1;
    }
    madvise(const_cast<char*>(r_data), r_size, MADV_WILLNEED);

    std::vector<CountryAgg> countries;
    countries.reserve(300);

    // Index 0 represents _UNMATCHED
    CountryAgg unmatched_agg;
    unmatched_agg.key = "_UNMATCHED";
    unmatched_agg.cid = 0;
    countries.push_back(std::move(unmatched_agg));

    std::unordered_map<std::string, uint16_t> country_map;
    country_map.reserve(300);

    const char* rp = r_data;
    const char* rend = r_data + r_size;

    // Skip header line
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
        while (rp < rend && *rp != '\r' && *rp != '\n') {
            ++rp;
        }
        std::string country_code(c_start, rp - c_start);
        if (rp < rend && *rp == '\r') ++rp;
        if (rp < rend && *rp == '\n') ++rp;

        if (country_code.empty()) continue;

        auto it = country_map.find(country_code);
        uint16_t cid;
        if (it == country_map.end()) {
            cid = static_cast<uint16_t>(countries.size());
            CountryAgg agg;
            agg.key = country_code;
            agg.cid = cid;
            countries.push_back(std::move(agg));
            country_map[country_code] = cid;
        } else {
            cid = it->second;
        }

        if (region_id < 10000000) {
            region_to_country[region_id] = cid;
        }
    }

    munmap(const_cast<char*>(r_data), r_size);
    close(r_fd);

    // Per-country name hash collection: zero string allocations!
    size_t num_countries = countries.size();
    std::vector<std::vector<uint64_t>> country_name_hashes(num_countries);
    for (size_t i = 0; i < num_countries; ++i) {
        country_name_hashes[i].reserve(16384);
    }

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
    if (rec_data == MAP_FAILED) {
        close(rec_fd);
        return 1;
    }
    madvise(const_cast<char*>(rec_data), rec_size, MADV_WILLNEED);

    const char* p = rec_data;
    const char* end = rec_data + rec_size;

    // Check BOM at start of file
    if (p + 3 <= end && static_cast<uint8_t>(p[0]) == 0xEF &&
        static_cast<uint8_t>(p[1]) == 0xBB && static_cast<uint8_t>(p[2]) == 0xBF) {
        p += 3;
    }

    // Skip header line
    while (p < end && *p != '\n') ++p;
    if (p < end && *p == '\n') ++p;

    while (p < end) {
        // Check for mid-file BOM at start of record
        if (p + 3 <= end && static_cast<uint8_t>(p[0]) == 0xEF &&
            static_cast<uint8_t>(p[1]) == 0xBB && static_cast<uint8_t>(p[2]) == 0xBF) {
            p += 3;
        }
        if (p >= end) break;

        // Field 0: id
        uint64_t record_id = 0;
        while (p < end && *p >= '0' && *p <= '9') {
            record_id = record_id * 10 + (*p - '0');
            ++p;
        }
        if (p < end && *p == ',') ++p;

        // Field 1: name (FNV-1a 64-bit zero-allocation hash & exact length)
        uint64_t name_hash = 14695981039346656037ULL;
        uint64_t name_len = 0;
        if (p < end && *p == '"') {
            ++p; // skip opening quote
            while (p < end) {
                if (*p == '"') {
                    if (p + 1 < end && *(p + 1) == '"') {
                        name_hash = (name_hash ^ static_cast<uint8_t>('"')) * 1099511628211ULL;
                        name_len += 1;
                        p += 2;
                    } else {
                        ++p; // closing quote
                        break;
                    }
                } else {
                    name_hash = (name_hash ^ static_cast<uint8_t>(*p)) * 1099511628211ULL;
                    name_len += 1;
                    ++p;
                }
            }
        } else {
            while (p < end && *p != ',') {
                name_hash = (name_hash ^ static_cast<uint8_t>(*p)) * 1099511628211ULL;
                name_len += 1;
                ++p;
            }
        }
        if (p < end && *p == ',') ++p;

        // Field 2: note (length in unescaped bytes only)
        uint64_t note_bytes = 0;
        if (p < end && *p == '"') {
            ++p; // skip opening quote
            while (p < end) {
                if (*p == '"') {
                    if (p + 1 < end && *(p + 1) == '"') {
                        note_bytes += 1;
                        p += 2;
                    } else {
                        ++p; // closing quote
                        break;
                    }
                } else {
                    note_bytes += 1;
                    ++p;
                }
            }
        } else {
            while (p < end && *p != ',') {
                note_bytes += 1;
                ++p;
            }
        }
        if (p < end && *p == ',') ++p;

        // Field 3: region_id
        uint32_t region_id = 0;
        while (p < end && *p >= '0' && *p <= '9') {
            region_id = region_id * 10 + (*p - '0');
            ++p;
        }
        if (p < end && *p == ',') ++p;

        // Field 4: amount
        bool negative = false;
        if (p < end && *p == '-') {
            negative = true;
            ++p;
        }
        int64_t whole = 0;
        while (p < end && *p >= '0' && *p <= '9') {
            whole = whole * 10 + (*p - '0');
            ++p;
        }
        if (p < end && *p == '.') ++p;
        int64_t frac = 0;
        if (p < end && *p >= '0' && *p <= '9') {
            frac = (*p - '0') * 10;
            ++p;
        }
        if (p < end && *p >= '0' && *p <= '9') {
            frac += (*p - '0');
            ++p;
        }
        int64_t cents = whole * 100 + frac;
        if (negative) cents = -cents;

        // Skip line terminator
        if (p < end && *p == '\r') ++p;
        if (p < end && *p == '\n') ++p;

        uint16_t cid = 0;
        if (region_id < 10000000) {
            cid = region_to_country[region_id];
        }

        auto& agg = countries[cid];
        if (agg.count == 0) {
            agg.amount_min = cents;
            agg.amount_max = cents;
        } else {
            if (cents < agg.amount_min) agg.amount_min = cents;
            if (cents > agg.amount_max) agg.amount_max = cents;
        }
        agg.count += 1;
        agg.id_sum += record_id;
        agg.amount_sum += cents;
        agg.name_bytes += name_len;
        agg.note_bytes += note_bytes;
        country_name_hashes[cid].push_back(name_hash);
    }

    munmap(const_cast<char*>(rec_data), rec_size);
    close(rec_fd);

    // 3. Count distinct names via fast in-place vector sort & unique
    std::vector<CountryAgg*> active_countries;
    for (auto& c : countries) {
        if (c.count > 0) {
            auto& v = country_name_hashes[c.cid];
            std::sort(v.begin(), v.end());
            v.erase(std::unique(v.begin(), v.end()), v.end());
            c.distinct_names = v.size();
            active_countries.push_back(&c);
        }
    }

    // Sort active countries by key ascending
    std::sort(active_countries.begin(), active_countries.end(), [](const CountryAgg* a, const CountryAgg* b) {
        return a->key < b->key;
    });

    // 4. Output aggregates.txt
    mkdir(out_dir.c_str(), 0777);
    std::string out_path = out_dir + "/aggregates.txt";
    int out_fd = open(out_path.c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0666);
    if (out_fd >= 0) {
        std::string buffer;
        buffer.reserve(active_countries.size() * 128);
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
        ssize_t written = write(out_fd, buffer.data(), buffer.size());
        (void)written;
        close(out_fd);
    }

    return 0;
}
