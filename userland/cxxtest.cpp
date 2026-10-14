// Exercises the C++ runtime on Nova: libstdc++, the C++ ABI (exceptions, RTTI,
// static construction/destruction, operator new/delete), STL and iostreams.
#include <algorithm>
#include <chrono>
#include <fstream>
#include <functional>
#include <iomanip>
#include <iostream>
#include <map>
#include <memory>
#include <numeric>
#include <sstream>
#include <stdexcept>
#include <string>
#include <unordered_map>
#include <vector>

static int passed = 0, failed = 0;
static void check(const std::string &name, bool ok) {
    (ok ? passed : failed)++;
    std::cout << (ok ? "  ok   " : "  FAIL ") << name << '\n';
}

struct Global {
    int value;
    Global() : value(1234) {}
    ~Global() { std::cout << "cxxtest: static destructor ran\n"; }
} global;

struct Shape {
    virtual ~Shape() = default;
    virtual double area() const = 0;
    virtual std::string name() const { return "shape"; }
};
struct Square : Shape {
    double s;
    explicit Square(double s) : s(s) {}
    double area() const override { return s * s; }
    std::string name() const override { return "square"; }
};
struct Circle : Shape {
    double r;
    explicit Circle(double r) : r(r) {}
    double area() const override { return 3.14159265 * r * r; }
};

struct NovaError : std::runtime_error {
    int code;
    NovaError(const std::string &m, int c) : std::runtime_error(m), code(c) {}
};

static int deep(int n) {
    if (n == 0) throw NovaError("from deep inside", 7);
    return deep(n - 1) + 1;
}

int main() {
    std::cout << "cxxtest: C++ runtime checks\n";
    check("static constructor", global.value == 1234);

    std::vector<int> v(1000);
    std::iota(v.begin(), v.end(), 1);
    check("vector + iota + accumulate", std::accumulate(v.begin(), v.end(), 0LL) == 500500);
    std::sort(v.begin(), v.end(), std::greater<int>());
    check("sort with comparator", v.front() == 1000 && v.back() == 1);

    std::map<std::string, int> m{{"b", 2}, {"a", 1}, {"c", 3}};
    check("std::map ordering", m.begin()->first == "a" && m.size() == 3);
    std::unordered_map<int, std::string> um;
    for (int i = 0; i < 5000; i++) um[i] = std::to_string(i * i);
    check("unordered_map 5000 entries", um.size() == 5000 && um[70] == "4900");

    std::string s = "Hello";
    s += ", Nova";
    std::transform(s.begin(), s.end(), s.begin(), ::toupper);
    check("std::string", s == "HELLO, NOVA" && s.find("NOVA") == 7);

    try {
        deep(50);
        check("exception thrown through 50 frames", false);
    } catch (const NovaError &e) {
        check("exception thrown through 50 frames", e.code == 7 && std::string(e.what()) == "from deep inside");
    }
    try {
        std::vector<int> small(3);
        (void)small.at(10);
        check("std::out_of_range from at()", false);
    } catch (const std::out_of_range &) {
        check("std::out_of_range from at()", true);
    }

    std::vector<std::unique_ptr<Shape>> shapes;
    shapes.push_back(std::make_unique<Square>(3));
    shapes.push_back(std::make_unique<Circle>(1));
    double total = 0;
    for (auto &p : shapes) total += p->area();
    check("virtual calls", total > 12.14 && total < 12.15 && shapes[0]->name() == "square" && shapes[1]->name() == "shape");
    check("dynamic_cast / RTTI", dynamic_cast<Square *>(shapes[0].get()) && !dynamic_cast<Square *>(shapes[1].get()) &&
                                     std::string(typeid(*shapes[1]).name()).find("Circle") != std::string::npos);

    auto shared = std::make_shared<int>(5);
    std::weak_ptr<int> weak = shared;
    { auto copy = shared; check("shared_ptr use_count", shared.use_count() == 2); }
    shared.reset();
    check("weak_ptr expires", weak.expired());

    int counter = 0;
    std::function<void(int)> add = [&counter](int n) { counter += n; };
    for (int i = 1; i <= 10; i++) add(i);
    check("lambda + std::function", counter == 55);

    std::ostringstream os;
    os << std::fixed << std::setprecision(2) << 3.14159 << ' ' << std::hex << 255;
    check("iostream formatting", os.str() == "3.14 ff");

    {
        std::ofstream out("/tmp/cxx.txt");
        out << "alpha\nbeta\ngamma\n";
    }
    std::ifstream in("/tmp/cxx.txt");
    std::vector<std::string> lines;
    for (std::string line; std::getline(in, line);) lines.push_back(line);
    check("fstream write and read", lines.size() == 3 && lines[2] == "gamma");
    std::remove("/tmp/cxx.txt");

    auto t0 = std::chrono::steady_clock::now();
    volatile double x = 0;
    for (int i = 0; i < 2000000; i++) x = x + i * 0.5;
    auto t1 = std::chrono::steady_clock::now();
    check("chrono steady_clock", t1 >= t0);

    auto *array = new long[1 << 20];
    array[(1 << 20) - 1] = 99;
    check("new[] 8 MiB", array[(1 << 20) - 1] == 99);
    delete[] array;

    std::cout << "cxxtest: " << passed << " passed, " << failed << " failed" << std::endl;
    return failed;
}
