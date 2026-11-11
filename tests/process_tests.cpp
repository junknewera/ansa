#include "omh/process.hpp"
#include "omh/local_storage.hpp"
#include <atomic>
#include <chrono>
#include <csignal>
#include <fstream>
#include <iostream>
#include <sys/prctl.h>
#include <sys/wait.h>
#include <thread>
#include <unistd.h>

namespace {
using namespace omh;
using namespace std::chrono_literals;
void check(bool value, const char* message) { if (!value) throw std::runtime_error(message); }
struct Temp {
    std::filesystem::path root;
    Temp() {
        char name[] = "/tmp/ansa-process-XXXXXX";
        const auto path = ::mkdtemp(name); check(path, "mkdtemp"); root = path;
    }
    ~Temp() { std::error_code error; std::filesystem::remove_all(root, error); }
};
ToolInvocation invocation(std::string id = "operation") { return {std::move(id), "call", "execute", "1", Json::parse("{}")}; }
ProcessOptions options(const Temp& temp, std::string command, std::size_t timeout = 3000) {
    return {temp.root, {OMH_TEST_SHELL, "-c", std::move(command)}, timeout, 16384};
}
void wait_file(const std::filesystem::path& path) {
    const auto deadline = std::chrono::steady_clock::now() + 5s;
    while (!std::filesystem::exists(path)) {
        check(std::chrono::steady_clock::now() < deadline, "process did not start");
        std::this_thread::sleep_for(5ms);
    }
}
void basic() {
    Temp temp; ProcessRunner runner(temp.root / "jobs");
    const auto spec = options(temp, "printf hello; printf diagnostic >&2; printf x >> marker; exit 7");
    const auto result = runner.run(invocation(), spec, {});
    check(result.execution == Execution::succeeded && result.effects == Effects::known_result, "nonzero exit lost");
    const auto output = Json::parse(result.output);
    check(output.index("exit_code") == 7 && output.text("stdout") == "hello" && output.text("stderr") == "diagnostic",
          "stdout/stderr/exit mismatch");
    check(runner.run(invocation(), spec, {}).output == result.output, "durable result was not reused");
    check(std::filesystem::file_size(temp.root / "marker") == 1, "process repeated");
    check(process_worker(temp.root / "jobs" / sha256(invocation().operation_id)) == 0 &&
          std::filesystem::file_size(temp.root / "marker") == 1, "internal worker repeated a completed process");
    auto changed = spec; changed.argv.back() = "printf bad >> marker";
    bool rejected = false;
    try { (void)runner.run(invocation(), changed, {}); } catch (const Error&) { rejected = true; }
    check(rejected && std::filesystem::file_size(temp.root / "marker") == 1, "operation changed after restart");
}
void output_limit() {
    Temp temp; ProcessRunner runner(temp.root / "jobs");
    ProcessOptions spec{temp.root, {std::filesystem::canonical("/proc/self/exe").string(), "--emit"}, 5000, 1024};
    const auto result = Json::parse(runner.run(invocation(), spec, {}).output);
    check(result.at("truncated") == Json::parse("true") && result.index("stdout_bytes") > 1024 &&
          result.index("stderr_bytes") > 1024 && result.index("exit_code") == 0,
          "pipe flood blocked or escaped bounds");
    check(result.text("stdout").size() + result.text("stderr").size() <= 3072, "invalid UTF-8 grew without bound");
}
void cancellation(bool timeout) {
    Temp temp; ProcessRunner runner(temp.root / "jobs");
    const auto spec = options(temp, "printf $$ > primary; sleep 30 & printf $! > descendant; wait", timeout ? 100 : 30000);
    std::stop_source stop;
    std::optional<ToolOutcome> result;
    std::jthread worker([&] { result = runner.run(invocation(), spec, stop.get_token()); });
    wait_file(temp.root / "descendant");
    const auto start = std::chrono::steady_clock::now();
    if (!timeout) stop.request_stop();
    worker.join();
    check(std::chrono::steady_clock::now() - start < 3s, "process cancellation blocked");
    const auto output = Json::parse(result->output);
    check(result->effects == Effects::known_result && output.at(timeout ? "timed_out" : "cancelled") == Json::parse("true") &&
          output.at("cleanup_complete") == Json::parse("true"), "cancellation result lost");
    for (const char* name : {"primary", "descendant"}) {
        std::ifstream file(temp.root / name); pid_t pid = 0; file >> pid;
        check(pid > 0 && ::kill(pid, 0) < 0 && errno == ESRCH, "owned process survived cancellation");
    }
}
void privileges() {
    Temp temp; ProcessRunner runner(temp.root / "jobs");
    const auto executable = std::filesystem::canonical("/proc/self/exe").string();
    ProcessOptions spec{temp.root, {executable, "--security"}, 3000, 4096};
    const auto outcome = Json::parse(runner.run(invocation(), spec, {}).output);
    const auto status = Json::parse(outcome.text("stdout"));
    check(status.index("no_new_privs") == 1 && status.at("tty") == Json::parse("false") &&
          status.index("uid") != 0, "child could acquire elevated privileges or read terminal input");
    spec.argv = {"sudo", "unreachable"};
    check(Json::parse(runner.run(invocation("sudo"), spec, {}).output).text("error") == "sudo_disabled", "sudo dispatched");
}
void crash() {
    Temp temp;
    check(::prctl(PR_SET_CHILD_SUBREAPER, 1L, 0L, 0L, 0L) == 0, "test subreaper");
    const auto child = ::fork(); check(child >= 0, "fork");
    if (child == 0) {
        ProcessRunner runner(temp.root / "jobs");
        (void)runner.run(invocation(), options(temp, "printf $$ > primary; sleep 30 & printf $! > descendant; wait", 30000), {});
        ::_exit(12);
    }
    wait_file(temp.root / "descendant");
    (void)::kill(child, SIGKILL);
    int status = 0;
    check(::waitpid(child, &status, 0) == child && WIFSIGNALED(status), "runtime crash");
    ProcessRunner recovered(temp.root / "jobs");
    const auto result = recovered.recover(invocation());
    check(result && result->effects == Effects::known_result &&
          Json::parse(result->output).at("cancelled") == Json::parse("true"), "supervisor did not preserve crash result");
    while (::waitpid(-1, &status, 0) < 0 && errno == EINTR) {}
    std::ifstream file(temp.root / "descendant"); pid_t descendant = 0; file >> descendant;
    check(descendant > 0 && ::kill(descendant, 0) < 0 && errno == ESRCH, "descendant survived runtime crash");
}
void detached() {
    Temp temp; ProcessRunner runner(temp.root / "jobs");
    ProcessOptions spec{temp.root, {std::filesystem::canonical("/proc/self/exe").string(), "--detach"}, 3000, 4096};
    const auto result = Json::parse(runner.run(invocation(), spec, {}).output);
    check(result.at("cleanup_complete") == Json::parse("true"), "detached descendant cleanup failed");
    const auto descendant = static_cast<pid_t>(std::stol(result.text("stdout")));
    check(descendant > 0 && ::kill(descendant, 0) < 0 && errno == ESRCH, "detached descendant survived");
}
void signals() {
    Temp temp; ProcessRunner runner(temp.root / "jobs");
    struct sigaction ignore{}, previous{};
    ignore.sa_handler = SIG_IGN; ignore.sa_flags = SA_NOCLDWAIT; ::sigemptyset(&ignore.sa_mask);
    check(::sigaction(SIGCHLD, &ignore, &previous) == 0, "sigaction fixture");
    const auto outcome = runner.run(invocation(), options(temp, "exit 7"), {});
    check(::sigaction(SIGCHLD, &previous, nullptr) == 0, "restore sigaction");
    check(outcome.effects == Effects::unknown || Json::parse(outcome.output).index("exit_code") == 7,
          "inherited SIGCHLD produced false successful result");
    const auto recovered = runner.recover(invocation());
    check(recovered && recovered->effects == Effects::known_result && Json::parse(recovered->output).index("exit_code") == 7,
          "fresh supervisor did not reset inherited SIGCHLD");
    auto spec = options(temp, "printf forbidden > unexpected");
    spec.cwd = std::filesystem::path(temp.root.string() + std::string("\0suffix", 7));
    bool rejected = false;
    try { (void)runner.run(invocation("nul"), spec, {}); } catch (const Error&) { rejected = true; }
    check(rejected && !std::filesystem::exists(temp.root / "unexpected"), "NUL cwd reached exec");
}
void evidence_admission() {
    Temp temp; ProcessRunner runner(temp.root / "jobs");
    std::stop_source stop;
    std::optional<ToolOutcome> outcome;
    std::jthread running([&] { outcome = runner.run(invocation(), options(temp, "printf x >> marker; printf ready > ready; sleep 30", 30000), stop.get_token()); });
    wait_file(temp.root / "ready");
    const auto job = temp.root / "jobs" / sha256(invocation().operation_id);
    // A parallel internal worker must not obtain a second launch.
    const auto child = ::fork(); check(child >= 0, "fork worker");
    if (child == 0) ::_exit(process_worker(job));
    int status = 0; check(::waitpid(child, &status, 0) == child && WEXITSTATUS(status) == 125, "concurrent worker admitted");
    stop.request_stop(); running.join();
    check(outcome && outcome->effects == Effects::known_result && std::filesystem::file_size(temp.root / "marker") == 1,
          "concurrent worker repeated effect");
    // A consumed launch without a result cannot be retried, even manually.
    std::filesystem::rename(job / "result.json", job / "saved-result.json");
    check(process_worker(job) == 125 && std::filesystem::file_size(temp.root / "marker") == 1,
          "consumed launch replayed");
    std::filesystem::rename(job / "saved-result.json", job / "result.json");
    std::filesystem::rename(job / "started.json", job / "saved-started.json");
    bool rejected = false;
    try { (void)runner.recover(invocation()); } catch (const Error&) { rejected = true; }
    check(rejected, "result admitted without launch identity");
    auto directory = local::open_private_directory(job);
    local::create_json(directory.get(), "started.json", Json::parse(R"({"version":1,"request_hash":"wrong"})"));
    rejected = false;
    try { (void)runner.recover(invocation()); } catch (const Error&) { rejected = true; }
    check(rejected, "result admitted with conflicting launch identity");
}
void guardian_crash() {
    Temp temp; ProcessRunner runner(temp.root / "jobs");
    check(::prctl(PR_SET_CHILD_SUBREAPER, 1L, 0L, 0L, 0L) == 0, "test subreaper");
    std::optional<ToolOutcome> outcome;
    const auto spec = options(temp, "printf x >> marker; printf $$ > primary; printf $PPID > guardian; sleep 2", 5000);
    std::jthread running([&] { outcome = runner.run(invocation(), spec, {}); });
    wait_file(temp.root / "guardian");
    std::ifstream input(temp.root / "guardian"); pid_t guardian = 0; input >> guardian;
    check(guardian > 0 && ::kill(guardian, SIGKILL) == 0, "guardian kill"); running.join();
    check(outcome && outcome->effects == Effects::unknown, "dead supervisor reported known cleanup");
    const auto job = temp.root / "jobs" / sha256(invocation().operation_id);
    check(process_worker(job) == 125, "dead supervisor launch was replayed");
    check(runner.run(invocation(), spec, {}).effects == Effects::unknown &&
          std::filesystem::file_size(temp.root / "marker") == 1, "unknown command was repeated");
    int status;
    while (::waitpid(-1, &status, 0) > 0 || errno == EINTR) {}
}
}
int main(int argc, char** argv) {
    if (argc == 3 && std::string_view(argv[1]) == "--process-worker") return omh::process_worker(argv[2]);
    if (argc == 2 && std::string_view(argv[1]) == "--emit") {
        const std::string bytes(4096, static_cast<char>(0xff));
        for (int n = 0; n < 64; ++n) { omh::local::write_all(1, bytes); omh::local::write_all(2, bytes); }
        return 0;
    }
    if (argc == 2 && std::string_view(argv[1]) == "--security") {
        std::cout << "{\"no_new_privs\":" << ::prctl(PR_GET_NO_NEW_PRIVS, 0L, 0L, 0L, 0L)
                  << ",\"uid\":" << ::geteuid() << ",\"tty\":" << (::isatty(0) ? "true" : "false") << "}\n";
        return 0;
    }
    if (argc == 2 && std::string_view(argv[1]) == "--detach") {
        const auto child = ::fork();
        if (child == 0) { (void)::setsid(); ::sleep(30); ::_exit(0); }
        std::cout << child << '\n';
        return 0;
    }
    try {
        check(argc == 2, "suite required");
        const std::string_view suite = argv[1];
        if (suite == "basic") basic();
        else if (suite == "output") output_limit();
        else if (suite == "cancel") cancellation(false);
        else if (suite == "timeout") cancellation(true);
        else if (suite == "privileges") privileges();
        else if (suite == "crash") crash();
        else if (suite == "detached") detached();
        else if (suite == "signals") signals();
        else if (suite == "evidence") evidence_admission();
        else if (suite == "guardian-crash") guardian_crash();
        else throw std::runtime_error("unknown suite");
        return 0;
    } catch (const std::exception& error) { std::cerr << error.what() << '\n'; return 1; }
}
