#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#include <algorithm>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <string>
#include <thread>
#include <vector>

struct Edge {
    uint32_t to;
    uint32_t weight;
};

struct Query {
    uint32_t s;
    uint32_t t;
};

struct EdgeTriple {
    uint32_t u;
    uint32_t v;
    uint32_t w;
};

// 4-ary Min-Heap for maximum CPU L1 cache line efficiency
struct MinHeap {
    struct Element {
        uint64_t dist;
        uint32_t u;
    };
    std::vector<Element> data;

    void reserve(size_t n) { data.reserve(n); }
    void clear() { data.clear(); }
    bool empty() const { return data.empty(); }
    const Element& top() const { return data[0]; }

    inline void push(uint64_t dist, uint32_t u) {
        size_t idx = data.size();
        data.push_back({dist, u});
        while (idx > 0) {
            size_t parent = (idx - 1) >> 2;
            if (data[idx].dist < data[parent].dist) {
                std::swap(data[idx], data[parent]);
                idx = parent;
            } else break;
        }
    }

    inline void pop() {
        data[0] = data.back();
        data.pop_back();
        if (data.empty()) return;
        size_t idx = 0;
        size_t size = data.size();
        while (true) {
            size_t best = idx;
            size_t child = (idx << 2) + 1;
            for (size_t c = child; c < child + 4 && c < size; ++c) {
                if (data[c].dist < data[best].dist) best = c;
            }
            if (best != idx) {
                std::swap(data[idx], data[best]);
                idx = best;
            } else break;
        }
    }
};

static inline const char* parse_u32(const char* p, const char* end, uint32_t& val) {
    while (p < end && (*p == ' ' || *p == '\n' || *p == '\r')) ++p;
    uint64_t res = 0;
    while (p < end && *p >= '0' && *p <= '9') {
        res = res * 10 + (*p - '0');
        ++p;
    }
    val = static_cast<uint32_t>(res);
    return p;
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

    std::string graph_path = data_dir + "/graph.txt";
    std::string queries_path = data_dir + "/queries.txt";
    std::string out_path = out_dir + "/distances.txt";

    // 1. Read graph.txt
    int g_fd = open(graph_path.c_str(), O_RDONLY);
    if (g_fd < 0) return 1;

    struct stat g_st;
    if (fstat(g_fd, &g_st) < 0 || g_st.st_size == 0) {
        close(g_fd);
        return 1;
    }

    size_t g_size = g_st.st_size;
    const char* g_data = static_cast<const char*>(mmap(nullptr, g_size, PROT_READ, MAP_PRIVATE, g_fd, 0));
    madvise(const_cast<char*>(g_data), g_size, MADV_WILLNEED);

    const char* gp = g_data;
    const char* g_end = g_data + g_size;

    uint32_t n = 0, m = 0;
    gp = parse_u32(gp, g_end, n);
    gp = parse_u32(gp, g_end, m);
    size_t edges_start_offset = gp - g_data;

    // Parallel multi-threaded chunk parsing for graph.txt
    unsigned int parse_threads = 12;
    std::vector<size_t> chunk_offsets(parse_threads + 1);
    chunk_offsets[0] = edges_start_offset;
    chunk_offsets[parse_threads] = g_size;
    size_t payload = g_size - edges_start_offset;

    for (unsigned int t = 1; t < parse_threads; ++t) {
        size_t off = edges_start_offset + t * (payload / parse_threads);
        while (off < g_size && g_data[off] != '\n') ++off;
        if (off < g_size) ++off;
        chunk_offsets[t] = off;
    }

    std::vector<std::vector<EdgeTriple>> thread_edges(parse_threads);
    std::vector<std::thread> parse_workers;
    for (unsigned int t = 0; t < parse_threads; ++t) {
        parse_workers.emplace_back([&, t]() {
            const char* cp = g_data + chunk_offsets[t];
            const char* cend = g_data + chunk_offsets[t + 1];
            thread_edges[t].reserve(m / parse_threads + 1024);
            while (cp < cend) {
                uint32_t u = 0, v = 0, w = 0;
                cp = parse_u32(cp, cend, u);
                if (cp >= cend) break;
                cp = parse_u32(cp, cend, v);
                if (cp >= cend) break;
                cp = parse_u32(cp, cend, w);
                thread_edges[t].push_back({u, v, w});
            }
        });
    }

    for (auto& pw : parse_workers) {
        pw.join();
    }

    munmap(const_cast<char*>(g_data), g_size);
    close(g_fd);

    // Build degree arrays
    std::vector<uint32_t> degree(n, 0);
    std::vector<uint32_t> rev_degree(n, 0);
    for (unsigned int t = 0; t < parse_threads; ++t) {
        for (const auto& e : thread_edges[t]) {
            degree[e.u]++;
            rev_degree[e.v]++;
        }
    }

    // CSR head offsets
    std::vector<uint32_t> head(n + 1, 0);
    std::vector<uint32_t> rev_head(n + 1, 0);
    for (uint32_t i = 0; i < n; ++i) {
        head[i + 1] = head[i] + degree[i];
        rev_head[i + 1] = rev_head[i] + rev_degree[i];
    }
    degree.clear();
    degree.shrink_to_fit();
    rev_degree.clear();
    rev_degree.shrink_to_fit();

    std::vector<Edge> edges(m);
    std::vector<Edge> rev_edges(m);
    std::vector<uint32_t> cur_pos = head;
    std::vector<uint32_t> rev_cur_pos = rev_head;

    for (unsigned int t = 0; t < parse_threads; ++t) {
        for (const auto& e : thread_edges[t]) {
            edges[cur_pos[e.u]++] = {e.v, e.w};
            rev_edges[rev_cur_pos[e.v]++] = {e.u, e.w};
        }
        thread_edges[t].clear();
        thread_edges[t].shrink_to_fit();
    }
    thread_edges.clear();
    thread_edges.shrink_to_fit();

    // 2. Read queries.txt
    int q_fd = open(queries_path.c_str(), O_RDONLY);
    if (q_fd < 0) return 1;

    struct stat q_st;
    if (fstat(q_fd, &q_st) < 0 || q_st.st_size == 0) {
        close(q_fd);
        return 1;
    }

    size_t q_size = q_st.st_size;
    const char* q_data = static_cast<const char*>(mmap(nullptr, q_size, PROT_READ, MAP_PRIVATE, q_fd, 0));

    const char* qp = q_data;
    const char* q_end = q_data + q_size;

    std::vector<Query> queries;
    queries.reserve(2500);

    while (qp < q_end) {
        uint32_t s = 0, t = 0;
        qp = parse_u32(qp, q_end, s);
        if (qp >= q_end) break;
        qp = parse_u32(qp, q_end, t);
        queries.push_back({s, t});
    }

    munmap(const_cast<char*>(q_data), q_size);
    close(q_fd);

    size_t num_queries = queries.size();
    std::vector<int64_t> answers(num_queries, -1);

    // 3. Parallel Bidirectional Dijkstra with 4 P-core threads and O(touched) state reset
    unsigned int num_workers = 4;
    if (num_queries < num_workers) num_workers = static_cast<unsigned int>(num_queries);
    if (num_workers == 0) num_workers = 1;

    size_t per_worker = (num_queries + num_workers - 1) / num_workers;
    std::vector<std::thread> workers;

    for (unsigned int tid = 0; tid < num_workers; ++tid) {
        size_t start_q = tid * per_worker;
        size_t end_q = std::min(num_queries, start_q + per_worker);

        workers.emplace_back([&, start_q, end_q]() {
            if (start_q >= end_q) return;
            std::vector<uint64_t> dist_f(n, UINT64_MAX);
            std::vector<uint64_t> dist_b(n, UINT64_MAX);
            std::vector<uint32_t> touched_f;
            std::vector<uint32_t> touched_b;
            touched_f.reserve(8192);
            touched_b.reserve(8192);

            MinHeap pq_f;
            MinHeap pq_b;
            pq_f.reserve(8192);
            pq_b.reserve(8192);

            for (size_t qi = start_q; qi < end_q; ++qi) {
                uint32_t s = queries[qi].s;
                uint32_t t = queries[qi].t;

                if (s == t) {
                    answers[qi] = 0;
                    continue;
                }

                dist_f[s] = 0;
                touched_f.push_back(s);
                pq_f.push(0, s);

                dist_b[t] = 0;
                touched_b.push_back(t);
                pq_b.push(0, t);

                uint64_t mu = UINT64_MAX;

                while (!pq_f.empty() && !pq_b.empty()) {
                    if (pq_f.top().dist + pq_b.top().dist >= mu) {
                        break;
                    }

                    if (pq_f.top().dist <= pq_b.top().dist) {
                        auto [df, u] = pq_f.top();
                        pq_f.pop();
                        if (df > dist_f[u]) continue;

                        uint32_t e_start = head[u];
                        uint32_t e_end = head[u + 1];
                        for (uint32_t ei = e_start; ei < e_end; ++ei) {
                            uint32_t v = edges[ei].to;
                            uint64_t nd = df + edges[ei].weight;
                            if (nd < dist_f[v]) {
                                if (dist_f[v] == UINT64_MAX) touched_f.push_back(v);
                                dist_f[v] = nd;
                                pq_f.push(nd, v);
                                if (dist_b[v] != UINT64_MAX) {
                                    uint64_t cand = nd + dist_b[v];
                                    if (cand < mu) mu = cand;
                                }
                            }
                        }
                    } else {
                        auto [db, u] = pq_b.top();
                        pq_b.pop();
                        if (db > dist_b[u]) continue;

                        uint32_t e_start = rev_head[u];
                        uint32_t e_end = rev_head[u + 1];
                        for (uint32_t ei = e_start; ei < e_end; ++ei) {
                            uint32_t v = rev_edges[ei].to;
                            uint64_t nd = db + rev_edges[ei].weight;
                            if (nd < dist_b[v]) {
                                if (dist_b[v] == UINT64_MAX) touched_b.push_back(v);
                                dist_b[v] = nd;
                                pq_b.push(nd, v);
                                if (dist_f[v] != UINT64_MAX) {
                                    uint64_t cand = nd + dist_f[v];
                                    if (cand < mu) mu = cand;
                                }
                            }
                        }
                    }
                }

                answers[qi] = (mu != UINT64_MAX) ? static_cast<int64_t>(mu) : -1;

                // Ultra-fast state reset: reset only touched vertices in microseconds!
                for (uint32_t u : touched_f) dist_f[u] = UINT64_MAX;
                for (uint32_t u : touched_b) dist_b[u] = UINT64_MAX;
                touched_f.clear();
                touched_b.clear();
                pq_f.clear();
                pq_b.clear();
            }
        });
    }

    for (auto& w : workers) {
        w.join();
    }

    // 4. Output distances.txt
    int out_fd = open(out_path.c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0666);
    if (out_fd >= 0) {
        std::string buffer;
        buffer.reserve(32 * 1024);
        for (int64_t d : answers) {
            buffer.append(std::to_string(d));
            buffer.push_back('\n');
        }
        ssize_t written = write(out_fd, buffer.data(), buffer.size());
        (void)written;
        close(out_fd);
    }

    return 0;
}
