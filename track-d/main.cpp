#include <iostream>
#include <fstream>
#include <vector>
#include <queue>
#include <string>
#include <algorithm>
#include <thread>
#include <cstdint>
#include <cstring>
#include <sys/mman.h>
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>

struct Edge {
    uint32_t to;
    uint32_t weight;
};

struct Query {
    uint32_t s;
    uint32_t t;
};

// Fast unsigned integer parsing
static inline const char* parse_u32(const char* p, const char* end, uint32_t& val) {
    while (p < end && (*p == ' ' || *p == '\n' || *p == '\r')) ++p;
    uint64_t v = 0;
    while (p < end && *p >= '0' && *p <= '9') {
        v = v * 10 + (*p - '0');
        ++p;
    }
    val = static_cast<uint32_t>(v);
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
    if (g_data == MAP_FAILED) {
        close(g_fd);
        return 1;
    }
    madvise(const_cast<char*>(g_data), g_size, MADV_WILLNEED);

    const char* gp = g_data;
    const char* g_end = g_data + g_size;

    uint32_t n = 0, m = 0;
    gp = parse_u32(gp, g_end, n);
    gp = parse_u32(gp, g_end, m);

    // Temp raw edges to build CSR
    struct RawEdge {
        uint32_t u, v, w;
    };
    std::vector<RawEdge> raw_edges(m);
    std::vector<uint32_t> degree(n, 0);

    for (uint32_t i = 0; i < m; ++i) {
        uint32_t u = 0, v = 0, w = 0;
        gp = parse_u32(gp, g_end, u);
        gp = parse_u32(gp, g_end, v);
        gp = parse_u32(gp, g_end, w);
        raw_edges[i] = {u, v, w};
        degree[u]++;
    }
    munmap(const_cast<char*>(g_data), g_size);
    close(g_fd);

    // Build CSR
    std::vector<uint32_t> head(n + 1, 0);
    for (uint32_t i = 0; i < n; ++i) {
        head[i + 1] = head[i] + degree[i];
    }

    std::vector<Edge> edges(m);
    std::vector<uint32_t> cur_pos = head;
    for (uint32_t i = 0; i < m; ++i) {
        uint32_t u = raw_edges[i].u;
        uint32_t idx = cur_pos[u]++;
        edges[idx] = {raw_edges[i].v, raw_edges[i].w};
    }
    raw_edges.clear();
    raw_edges.shrink_to_fit();

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
    if (q_data == MAP_FAILED) {
        close(q_fd);
        return 1;
    }

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

    // 3. Parallel Dijkstra
    unsigned int num_threads = 12;
    unsigned int hw = std::thread::hardware_concurrency();
    if (hw > 0 && hw < num_threads) num_threads = hw;
    if (num_queries < num_threads) num_threads = static_cast<unsigned int>(num_queries);
    if (num_threads == 0) num_threads = 1;

    size_t per_worker = (num_queries + num_threads - 1) / num_threads;
    std::vector<std::thread> workers;

    for (unsigned int tid = 0; tid < num_threads; ++tid) {
        size_t start_q = tid * per_worker;
        size_t end_q = std::min(num_queries, start_q + per_worker);

        workers.emplace_back([&, start_q, end_q]() {
            // Thread-local generation distance array: O(1) query reset
            std::vector<uint32_t> seen(n, 0);
            std::vector<uint64_t> dist(n, 0);
            uint32_t query_id = 0;

            using HeapNode = std::pair<uint64_t, uint32_t>; // (dist, node)
            std::priority_queue<HeapNode, std::vector<HeapNode>, std::greater<HeapNode>> pq;

            for (size_t qi = start_q; qi < end_q; ++qi) {
                uint32_t s = queries[qi].s;
                uint32_t t = queries[qi].t;

                if (s == t) {
                    answers[qi] = 0;
                    continue;
                }

                query_id++;
                seen[s] = query_id;
                dist[s] = 0;
                pq.emplace(0, s);

                int64_t ans = -1;
                while (!pq.empty()) {
                    auto [d, u] = pq.top();
                    pq.pop();

                    if (d > dist[u]) continue;
                    if (u == t) {
                        ans = static_cast<int64_t>(d);
                        break;
                    }

                    uint32_t e_start = head[u];
                    uint32_t e_end = head[u + 1];

                    for (uint32_t ei = e_start; ei < e_end; ++ei) {
                        uint32_t v = edges[ei].to;
                        uint64_t nd = d + edges[ei].weight;

                        if (seen[v] != query_id || nd < dist[v]) {
                            seen[v] = query_id;
                            dist[v] = nd;
                            pq.emplace(nd, v);
                        }
                    }
                }

                // Clear heap if exited early
                while (!pq.empty()) pq.pop();
                answers[qi] = ans;
            }
        });
    }

    for (auto& w : workers) {
        w.join();
    }

    // 4. Write formatted output
    int out_fd = open(out_path.c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0666);
    if (out_fd >= 0) {
        std::string buffer;
        buffer.reserve(64 * 1024);
        for (int64_t ans : answers) {
            buffer.append(std::to_string(ans));
            buffer.push_back('\n');
            if (buffer.size() >= 32 * 1024) {
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
