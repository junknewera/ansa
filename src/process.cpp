#include "omh/process.hpp"
#include "omh/context_codec.hpp"
#include "omh/local_storage.hpp"
#include <algorithm>
#include <cerrno>
#include <chrono>
#include <climits>
#include <csignal>
#include <fcntl.h>
#include <fstream>
#include <linux/capability.h>
#include <poll.h>
#include <sstream>
#include <sys/prctl.h>
#include <sys/file.h>
#include <sys/syscall.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <thread>
#include <unistd.h>

extern char** environ;
namespace omh {
namespace {
using local::Fd;
using namespace std::chrono_literals;
Json number(std::uint64_t value) { return Json::parse(std::to_string(value)); }
Json boolean(bool value) { return Json::parse(value ? "true" : "false"); }
ToolOutcome unknown() { return {Execution::failed, Effects::unknown, R"({"error":"process_result_unknown"})"}; }
Json specification(const ToolInvocation& invocation, const ProcessOptions& options) {
    std::vector<Json> argv;
    std::size_t bytes = 0;
    require(options.cwd.is_absolute() && options.cwd.string().find('\0') == std::string::npos &&
            !options.argv.empty() && options.argv.size() <= 64 &&
            options.timeout_ms > 0 && options.timeout_ms <= 600000 &&
            options.output_bytes > 0 && options.output_bytes <= 16384, "process_options", Stage::execution);
    for (const auto& argument : options.argv) {
        require(argument.find('\0') == std::string::npos && argument.size() <= 16384 - bytes,
                "process_arguments", Stage::execution);
        bytes += argument.size(); argv.push_back(Json::string(argument));
    }
    require(!options.argv.front().empty(), "process_executable", Stage::execution);
    return Json::object({{"version", number(1)}, {"invocation", save_invocation(invocation)},
        {"cwd", Json::string(options.cwd.string())}, {"argv", Json::array(argv)},
        {"timeout_ms", number(options.timeout_ms)}, {"output_bytes", number(options.output_bytes)}});
}
ToolOutcome admitted_evidence(int directory, const ToolInvocation& invocation) {
    const auto spec = local::read_json(directory, "request.json");
    const auto started = local::read_json(directory, "started.json");
    const auto result = local::read_json(directory, "result.json");
    require(spec && started && result && spec->index("version") == 1 && started->index("version") == 1 &&
            started->text("request_hash") == sha256(spec->dump()) && result->index("version") == 1 &&
            spec->at("invocation") == save_invocation(invocation) &&
            result->text("request_hash") == sha256(spec->dump()), "process_evidence_identity", Stage::execution);
    const auto execution = result->index("execution"), effects = result->index("effects");
    require(execution <= static_cast<std::size_t>(Execution::denied) &&
            (effects == static_cast<std::size_t>(Effects::known_result) ||
             effects == static_cast<std::size_t>(Effects::unknown)), "process_evidence_outcome", Stage::execution);
    auto output = result->text("output");
    require(output.size() <= 128 * 1024, "process_evidence_limit", Stage::execution);
    return {static_cast<Execution>(execution), static_cast<Effects>(effects), std::move(output)};
}
void evidence(int directory, const Json& spec, const ToolOutcome& outcome) {
    local::create_json(directory, "result.json", Json::object({{"version", number(1)},
        {"request_hash", Json::string(sha256(spec.dump()))}, {"execution", number(static_cast<std::size_t>(outcome.execution))},
        {"effects", number(static_cast<std::size_t>(outcome.effects))}, {"output", Json::string(outcome.output)}}));
}
void pipe(Fd& read, Fd& write) {
    int values[2]; require(::pipe2(values, O_CLOEXEC) == 0, "process_pipe", Stage::execution);
    read.reset(values[0]); write.reset(values[1]);
}
void nonblocking(int fd) {
    const int flags = ::fcntl(fd, F_GETFL);
    require(flags >= 0 && ::fcntl(fd, F_SETFL, flags | O_NONBLOCK) == 0, "process_nonblocking", Stage::execution);
}
void child_failed(int descriptor, int error) {
    const auto written = ::write(descriptor, &error, sizeof(error));
    (void)written; ::_exit(127);
}
// Only syscalls between fork and exec; the runtime can have other threads.
bool restrict_child() {
    __user_cap_header_struct header{_LINUX_CAPABILITY_VERSION_3, 0};
    __user_cap_data_struct caps[2]{};
    return ::geteuid() != 0 && ::getuid() == ::geteuid() &&
        ::syscall(SYS_capset, &header, caps) == 0 &&
        ::prctl(PR_SET_NO_NEW_PRIVS, 1L, 0L, 0L, 0L) == 0;
}
std::vector<pid_t> direct_children() {
    std::ifstream input("/proc/self/task/" + std::to_string(::getpid()) + "/children");
    std::vector<pid_t> children;
    for (pid_t pid; input >> pid;) if (pid > 0) children.push_back(pid);
    return children;
}
bool cleanup(pid_t primary, int& status, bool& owned) {
    // primary is still waitable (WNOWAIT): its pid/pgid cannot be recycled here.
    if (owned) (void)::kill(-primary, SIGKILL);
    const auto deadline = std::chrono::steady_clock::now() + 1500ms;
    for (;;) {
        // A private subreaper also owns descendants which called setsid().
        for (const auto pid : direct_children()) (void)::kill(pid, SIGKILL);
        bool live = false;
        for (;;) {
            int value = 0;
            const auto reaped = ::waitpid(-1, &value, WNOHANG);
            if (reaped > 0) { if (reaped == primary) { status = value; owned = false; } continue; }
            if (reaped == 0) live = true;
            else if (errno == EINTR) continue;
            else if (errno != ECHILD) return false;
            break;
        }
        if (!live && direct_children().empty()) return !owned;
        if (std::chrono::steady_clock::now() >= deadline) return false;
        std::this_thread::sleep_for(5ms);
    }
}
struct ChildGuard {
    pid_t pid;
    bool owned = true;
    ~ChildGuard() {
        if (!pid) return;
        try { int status = 0; (void)cleanup(pid, status, owned); }
        catch (...) { if (owned) { (void)::kill(-pid, SIGKILL); (void)::kill(pid, SIGKILL); } }
    }
};
ToolOutcome supervise(const Json& spec) {
    require(::getuid() != 0 && ::geteuid() == ::getuid() &&
            ::prctl(PR_SET_CHILD_SUBREAPER, 1L, 0L, 0L, 0L) == 0, "process_privileges", Stage::execution);
    const auto cwd = spec.text("cwd");
    const auto timeout = spec.index("timeout_ms"), budget = spec.index("output_bytes");
    require(timeout > 0 && timeout <= 600000 && budget > 0 && budget <= 16384, "process_options", Stage::execution);
    std::vector<std::string> arguments;
    for (const auto& argument : spec.at("argv").elements()) arguments.push_back(argument.text());
    require(!arguments.empty() && arguments.size() <= 64, "process_arguments", Stage::execution);
    if (std::filesystem::path(arguments.front()).filename() == "sudo")
        return {Execution::succeeded, Effects::known_result, R"({"error":"sudo_disabled","exit_code":126})"};
    std::vector<char*> argv;
    for (auto& argument : arguments) argv.push_back(argument.data());
    argv.push_back(nullptr);
    Fd directory(::open(cwd.c_str(), O_RDONLY | O_DIRECTORY | O_CLOEXEC));
    require(directory.get() >= 0, "process_cwd", Stage::execution);
    Fd out_read, out_write, err_read, err_write, exec_read, exec_write;
    pipe(out_read, out_write); pipe(err_read, err_write); pipe(exec_read, exec_write);
    nonblocking(out_read.get()); nonblocking(err_read.get()); nonblocking(exec_read.get()); nonblocking(STDIN_FILENO);
    std::string out, err;
    out.reserve(budget); err.reserve(budget);
    Fd null(::open("/dev/null", O_RDONLY | O_CLOEXEC));
    require(null.get() >= 0, "process_stdin", Stage::execution);
    const auto supervisor_pid = ::getpid();
    const auto primary = ::fork();
    require(primary >= 0, "process_fork", Stage::execution);
    if (primary == 0) {
        if (::prctl(PR_SET_PDEATHSIG, SIGKILL, 0L, 0L, 0L) != 0 || ::getppid() != supervisor_pid ||
            ::setpgid(0, 0) != 0 || !restrict_child() || ::fchdir(directory.get()) != 0 ||
            ::dup2(null.get(), STDIN_FILENO) < 0 || ::dup2(out_write.get(), STDOUT_FILENO) < 0 ||
            ::dup2(err_write.get(), STDERR_FILENO) < 0) child_failed(exec_write.get(), errno);
        if (::syscall(SYS_close_range, 3U, UINT_MAX, 4U /* CLOSE_RANGE_CLOEXEC */) != 0)
            child_failed(exec_write.get(), errno);
        // execvp is safe here: this supervisor is single-threaded.
        ::execvp(argv[0], argv.data()); child_failed(exec_write.get(), errno);
    }
    ChildGuard guard{primary};
    (void)::setpgid(primary, primary);
    out_write.reset(); err_write.reset(); exec_write.reset();
    std::uint64_t out_bytes = 0, err_bytes = 0;
    bool truncated = false, cancelled = false, timed_out = false;
    int spawn_error = 0, status = 0;
    auto drain = [&](Fd& fd, std::string& saved, std::uint64_t& seen) {
        if (fd.get() < 0) return;
        char bytes[4096];
        for (unsigned chunk = 0; chunk < 16; ++chunk) {
            const auto count = ::read(fd.get(), bytes, sizeof(bytes));
            if (count < 0 && errno == EINTR) continue;
            if (count < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) break;
            if (count <= 0) { fd.reset(); break; }
            const auto size = static_cast<std::size_t>(count);
            seen += size;
            const auto keep = std::min(size, budget - out.size() - err.size());
            saved.append(bytes, keep); truncated = truncated || keep != size;
        }
    };
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(timeout);
    for (;;) {
        drain(out_read, out, out_bytes); drain(err_read, err, err_bytes);
        char control;
        const auto command = ::read(STDIN_FILENO, &control, 1);
        if (command == 0 || command > 0) cancelled = true;
        timed_out = std::chrono::steady_clock::now() >= deadline;
        siginfo_t info{};
        const auto waited = ::waitid(P_PID, static_cast<id_t>(primary), &info, WEXITED | WNOHANG | WNOWAIT);
        if (cancelled || timed_out || (waited == 0 && info.si_pid == primary)) break;
        if (waited < 0 && errno != EINTR) { cancelled = true; break; }
        pollfd waiting[]{{out_read.get(), POLLIN, 0}, {err_read.get(), POLLIN, 0}, {STDIN_FILENO, POLLIN | POLLHUP, 0}};
        (void)::poll(waiting, 3, 20);
    }
    const bool clean = cleanup(primary, status, guard.owned);
    guard.pid = 0;
    drain(out_read, out, out_bytes); drain(err_read, err, err_bytes);
    const auto exec_count = ::read(exec_read.get(), &spawn_error, sizeof(spawn_error));
    if (exec_count != static_cast<ssize_t>(sizeof(spawn_error))) spawn_error = 0;
    const int exit_code = WIFEXITED(status) ? WEXITSTATUS(status) : -1;
    const int signal = WIFSIGNALED(status) ? WTERMSIG(status) : 0;
    const auto output = Json::object({{"exit_code", Json::parse(std::to_string(exit_code))},
        {"signal", number(static_cast<unsigned>(signal))}, {"cancelled", boolean(cancelled)},
        {"timed_out", boolean(timed_out)}, {"truncated", boolean(truncated)},
        {"stdout", Json::string(local::utf8_text(out))}, {"stderr", Json::string(local::utf8_text(err))},
        {"stdout_bytes", number(out_bytes)}, {"stderr_bytes", number(err_bytes)},
        {"spawn_errno", number(static_cast<unsigned>(spawn_error))}, {"cleanup_complete", boolean(clean)}}).dump();
    return {clean ? Execution::succeeded : Execution::failed, clean ? Effects::known_result : Effects::unknown, output};
}
}

ProcessRunner::ProcessRunner(std::filesystem::path evidence_root, std::filesystem::path worker)
    : root_(std::filesystem::absolute(std::move(evidence_root))), worker_(std::filesystem::canonical(std::move(worker))) {
    local::private_directory(root_); (void)local::open_private_directory(root_);
}
ToolOutcome ProcessRunner::run(const ToolInvocation& invocation, const ProcessOptions& options, std::stop_token stop) const {
    if (stop.stop_requested()) return {Execution::cancelled, Effects::known_result, R"({"error":"cancelled"})"};
    const auto spec = specification(invocation, options);
    auto root = local::open_private_directory(root_);
    const auto name = sha256(invocation.operation_id);
    if (::mkdirat(root.get(), name.c_str(), 0700) != 0) {
        if (errno == EEXIST) {
            auto existing = local::open_private_directory(root_ / name);
            const auto request = local::read_json(existing.get(), "request.json");
            require(request && *request == spec, "process_operation_conflict", Stage::execution);
            const auto old = recover(invocation);
            return old.value_or(unknown());
        }
        fail(ErrorCode::tool, Stage::execution, "process_job_create");
    }
    local::sync(root.get());
    const auto path = root_ / name;
    auto directory = local::open_private_directory(path);
    local::create_json(directory.get(), "request.json", spec);
    Fd control_read, control_write; pipe(control_read, control_write);
    Fd null(::open("/dev/null", O_WRONLY | O_CLOEXEC));
    require(null.get() >= 0, "process_output", Stage::execution);
    const auto executable = worker_.string(), job = path.string();
    char flag[] = "--process-worker";
    char* argv[]{const_cast<char*>(executable.c_str()), flag, const_cast<char*>(job.c_str()), nullptr};
    const auto supervisor = ::fork();
    require(supervisor >= 0, "process_supervisor_fork", Stage::execution);
    if (supervisor == 0) {
        if (::setsid() < 0 || !restrict_child() || ::dup2(control_read.get(), STDIN_FILENO) < 0 ||
            ::dup2(null.get(), STDOUT_FILENO) < 0 || ::dup2(null.get(), STDERR_FILENO) < 0) ::_exit(125);
        if (::syscall(SYS_close_range, 3U, UINT_MAX, 4U) != 0) ::_exit(125);
        ::execve(executable.c_str(), argv, environ); ::_exit(125);
    }
    control_read.reset();
    int status = 0;
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(options.timeout_ms) + 5s;
    bool resumed_guardian = false;
    for (;;) {
        const auto done = ::waitpid(supervisor, &status, WNOHANG);
        if (done == supervisor) break;
        if (done < 0 && errno != EINTR) return unknown();
        if (stop.stop_requested()) control_write.reset();
        if (std::chrono::steady_clock::now() >= deadline) {
            if (!resumed_guardian) {
                // A stopped supervisor must get a chance to observe cancellation.
                control_write.reset();
                (void)::kill(supervisor, SIGCONT);
                resumed_guardian = true;
                std::this_thread::sleep_for(1600ms);
                continue;
            }
            (void)::kill(supervisor, SIGKILL);
            while (::waitpid(supervisor, &status, 0) < 0 && errno == EINTR) {}
            return unknown();
        }
        std::this_thread::sleep_for(10ms);
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) return unknown();
    return admitted_evidence(directory.get(), invocation);
}
std::optional<ToolOutcome> ProcessRunner::recover(const ToolInvocation& invocation) const {
    const auto path = root_ / sha256(invocation.operation_id);
    if (!std::filesystem::exists(path)) return {};
    auto directory = local::open_private_directory(path);
    for (unsigned attempt = 0; attempt < 60; ++attempt) {
        if (local::read_json(directory.get(), "result.json")) return admitted_evidence(directory.get(), invocation);
        std::this_thread::sleep_for(50ms);
    }
    return {};
}
int process_worker(const std::filesystem::path& job) {
    try {
        struct sigaction defaults{};
        defaults.sa_handler = SIG_DFL;
        ::sigemptyset(&defaults.sa_mask);
        for (const int signal : {SIGCHLD, SIGPIPE, SIGINT, SIGTERM, SIGHUP, SIGQUIT})
            require(::sigaction(signal, &defaults, nullptr) == 0, "process_signal_reset", Stage::execution);
        sigset_t unblocked;
        ::sigemptyset(&unblocked);
        require(::sigprocmask(SIG_SETMASK, &unblocked, nullptr) == 0, "process_signal_mask", Stage::execution);
        auto directory = local::open_private_directory(job);
        const auto spec = local::read_json(directory.get(), "request.json");
        require(spec && spec->index("version") == 1, "process_job_request", Stage::execution);
        Fd lock(::openat(directory.get(), "worker.lock", O_RDWR | O_CREAT | O_CLOEXEC | O_NOFOLLOW, 0600));
        struct stat lock_status{};
        require(lock.get() >= 0 && ::fstat(lock.get(), &lock_status) == 0 && S_ISREG(lock_status.st_mode) &&
                lock_status.st_uid == ::geteuid() && (lock_status.st_mode & 077) == 0 && lock_status.st_nlink == 1,
                "process_job_lock", Stage::execution);
        if (::flock(lock.get(), LOCK_EX | LOCK_NB) != 0) return 125;
        if (local::read_json(directory.get(), "result.json")) {
            (void)admitted_evidence(directory.get(), load_invocation(spec->at("invocation")));
            return 0;
        }
        if (local::read_json(directory.get(), "started.json")) return 125;
        // A consumed launch is never retried, even if its supervisor crashed.
        local::create_json(directory.get(), "started.json", Json::object({{"version", number(1)},
            {"request_hash", Json::string(sha256(spec->dump()))}}));
        ToolOutcome result = unknown();
        try { result = supervise(*spec); }
        catch (...) { result = unknown(); }
        evidence(directory.get(), *spec, result);
        return 0;
    } catch (...) { return 125; }
}
}
