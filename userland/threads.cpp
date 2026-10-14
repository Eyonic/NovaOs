// Exercises threads on Nova: clone, futex-based mutexes and condition variables,
// thread-local storage, std::async, and joining.
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <future>
#include <iostream>
#include <mutex>
#include <queue>
#include <set>
#include <thread>
#include <vector>

static int passed = 0, failed = 0;
static void check(const char *name, bool ok) {
    (ok ? passed : failed)++;
    std::cout << (ok ? "  ok   " : "  FAIL ") << name << std::endl;
}

thread_local int tls_value = 0;

int main() {
    std::cout << "threads: threading checks" << std::endl;

    std::atomic<long> atomic_count{0};
    long locked_count = 0;
    std::mutex m;
    std::vector<std::thread> workers;
    for (int t = 0; t < 8; t++) {
        workers.emplace_back([&] {
            for (int i = 0; i < 20000; i++) {
                atomic_count++;
                std::lock_guard<std::mutex> g(m);
                locked_count++;
            }
        });
    }
    for (auto &w : workers) w.join();
    check("8 threads, atomic counter", atomic_count == 160000);
    check("8 threads, mutex-protected counter", locked_count == 160000);

    std::queue<int> q;
    std::condition_variable cv;
    bool done = false;
    long sum = 0;
    std::thread consumer([&] {
        std::unique_lock<std::mutex> lk(m);
        for (;;) {
            cv.wait(lk, [&] { return !q.empty() || done; });
            while (!q.empty()) { sum += q.front(); q.pop(); }
            if (done) break;
        }
    });
    for (int i = 1; i <= 5000; i++) {
        { std::lock_guard<std::mutex> g(m); q.push(i); }
        cv.notify_one();
    }
    { std::lock_guard<std::mutex> g(m); done = true; }
    cv.notify_one();
    consumer.join();
    check("condition variable producer/consumer", sum == 5000L * 5001 / 2);

    std::set<int> seen;
    std::mutex sm;
    std::vector<std::thread> tl;
    for (int t = 1; t <= 4; t++)
        tl.emplace_back([&, t] { tls_value = t * 10; std::this_thread::yield(); std::lock_guard<std::mutex> g(sm); seen.insert(tls_value); });
    for (auto &t : tl) t.join();
    check("thread_local storage", seen.size() == 4 && tls_value == 0);

    auto f = std::async(std::launch::async, [] { long s = 0; for (int i = 1; i <= 1000; i++) s += i; return s; });
    check("std::async", f.get() == 500500);

    auto t0 = std::chrono::steady_clock::now();
    std::thread sleeper([] { std::this_thread::sleep_for(std::chrono::milliseconds(50)); });
    sleeper.join();
    auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(std::chrono::steady_clock::now() - t0).count();
    check("sleeping thread joins", ms >= 40 && ms < 2000);

    std::promise<std::string> p;
    std::thread thrower([&] {
        try { throw std::runtime_error("from a thread"); }
        catch (const std::exception &e) { p.set_value(e.what()); }
    });
    thrower.join();
    check("exceptions inside threads", p.get_future().get() == "from a thread");

    std::cout << "threads: " << passed << " passed, " << failed << " failed" << std::endl;
    return failed;
}
