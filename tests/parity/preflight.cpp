// Test-only Linux prerequisite probe. No agent/config/auth initialization.
#include <cerrno>
#include <chrono>
#include <csignal>
#include <cstring>
#include <fcntl.h>
#include <fstream>
#include <iomanip>
#include <iostream>
#include <linux/capability.h>
#include <linux/fs.h>
#include <poll.h>
#include <string>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <thread>
#include <unistd.h>
#include <vector>

namespace {
struct Check { std::string id, status, detail; };
std::vector<Check> checks;
void record(const char* id, bool passed, int error = 0, const char* detail = "verification_failed") {
    const bool unavailable = error == ENOSYS || error == ENOENT || error == EOPNOTSUPP || error == EPERM || error == EACCES;
    checks.push_back({id, passed ? "pass" : unavailable ? "unsupported" : "fail",
        passed ? "verified" : error ? "errno=" + std::to_string(error) + ": " + std::strerror(error) : detail});
}
struct Fd {
    int value = -1;
    ~Fd() { reset(); }
    void reset() { if (value >= 0) { (void)::close(value); value = -1; } }
};
struct Pipe {
    Fd read, write;
    bool open() {
        int values[2];
        if (::pipe2(values, O_CLOEXEC) != 0) return false;
        read.value = values[0]; write.value = values[1]; return true;
    }
};
// Never signal a PID after it stops being our waitable child.
struct Child {
    pid_t pid = -1;
    ~Child() {
        if (pid <= 0) return;
        int status = 0; pid_t waited;
        do { waited = ::waitpid(pid, &status, WNOHANG); } while (waited < 0 && errno == EINTR);
        if (waited == 0) {
            (void)::kill(pid, SIGKILL);
            do { waited = ::waitpid(pid, &status, 0); } while (waited < 0 && errno == EINTR);
        }
    }
};
bool exited(pid_t pid, int code) {
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);
    do {
        siginfo_t info{};
        if (::waitid(P_PID, static_cast<id_t>(pid), &info, WEXITED | WNOWAIT | WNOHANG) < 0) {
            if (errno == EINTR) continue;
            return false;
        }
        if (info.si_pid == pid) {
            if (info.si_code == CLD_EXITED && info.si_status == code) return true;
            errno = EPROTO; return false;
        }
        std::this_thread::sleep_for(std::chrono::milliseconds(2));
    } while (std::chrono::steady_clock::now() < deadline);
    errno = ETIMEDOUT; return false;
}
void await_eof(int fd) {
    char byte;
    while (::read(fd, &byte, 1) < 0 && errno == EINTR) {}
}
void proc_and_wait() {
    Pipe release;
    if (!release.open()) {
        const int error = errno; record("proc_children", false, error); record("waitid_wnowait", false, error); return;
    }
    Child child{::fork()};
    if (child.pid == 0) { release.write.reset(); await_eof(release.read.value); ::_exit(17); }
    if (child.pid < 0) {
        const int error = errno; record("proc_children", false, error); record("waitid_wnowait", false, error); return;
    }
    release.read.reset();
    const auto path = "/proc/self/task/" + std::to_string(::getpid()) + "/children";
    errno = 0;
    std::ifstream children(path);
    const int error = errno;
    bool found = false;
    for (pid_t pid; children >> pid;) if (pid == child.pid) found = true;
    record("proc_children", found, found ? 0 : error, "owned_child_missing");
    release.write.reset();
    const bool observed = exited(child.pid, 17);
    const int wait_error = errno;
    // A second observation proves WNOWAIT did not consume the exit status.
    record("waitid_wnowait", observed && exited(child.pid, 17), observed ? 0 : wait_error);
}
void subreaper() {
    int enabled = 0;
    if (::prctl(PR_SET_CHILD_SUBREAPER, 1L, 0L, 0L, 0L) != 0 ||
        ::prctl(PR_GET_CHILD_SUBREAPER, &enabled, 0L, 0L, 0L) != 0 || enabled != 1) {
        record("subreaper_adoption", false, errno); return;
    }
    Pipe report, release;
    if (!report.open() || !release.open()) { record("subreaper_adoption", false, errno); return; }
    Child middle{::fork()};
    if (middle.pid == 0) {
        const auto grandchild = ::fork();
        if (grandchild == 0) {
            report.read.reset(); release.write.reset();
            const auto pid = ::getpid();
            (void)::write(report.write.value, &pid, sizeof(pid)); report.write.reset();
            await_eof(release.read.value); ::_exit(23);
        }
        ::_exit(grandchild < 0 ? 24 : 0);
    }
    if (middle.pid < 0) { record("subreaper_adoption", false, errno); return; }
    report.write.reset(); release.read.reset();
    pid_t pid = -1;
    pollfd ready{report.read.value, POLLIN, 0};
    int polled;
    do { polled = ::poll(&ready, 1, 2000); } while (polled < 0 && errno == EINTR);
    const bool reported = polled > 0 && ::read(report.read.value, &pid, sizeof(pid)) == static_cast<ssize_t>(sizeof(pid)) && pid > 0;
    Child adopted{reported ? pid : -1};
    const bool parent_exited = exited(middle.pid, 0);
    int status = 0;
    // Middle has exited, so its live child must now be waitable by this subreaper.
    const bool owned = reported && parent_exited && ::waitpid(pid, &status, WNOHANG) == 0;
    release.write.reset(); // Unblocks even an unreported grandchild; it never runs arbitrary code.
    record("subreaper_adoption", owned && exited(pid, 23), 0, "grandchild_not_adopted_or_reaped");
    // If reporting failed, still reap all test-owned children after releasing them.
    if (!reported) { while (::waitpid(-1, &status, 0) > 0 || errno == EINTR) {} }
}
void restrictions() {
    record("unprivileged_uid", ::getuid() != 0 && ::getuid() == ::geteuid(), 0, "requires_nonroot_matching_uid");
    const bool nnp = ::prctl(PR_SET_NO_NEW_PRIVS, 1L, 0L, 0L, 0L) == 0 &&
        ::prctl(PR_GET_NO_NEW_PRIVS, 0L, 0L, 0L, 0L) == 1;
    record("no_new_privs", nnp, nnp ? 0 : errno);
    __user_cap_header_struct header{_LINUX_CAPABILITY_VERSION_3, 0};
    __user_cap_data_struct before[2]{}, zero[2]{}, after[2]{};
    const bool operated = ::syscall(SYS_capget, &header, before) == 0 &&
        ::syscall(SYS_capset, &header, zero) == 0 && ::syscall(SYS_capget, &header, after) == 0;
    const bool cleared = operated && after[0].effective == 0 && after[0].permitted == 0 && after[0].inheritable == 0 &&
        after[1].effective == 0 && after[1].permitted == 0 && after[1].inheritable == 0;
    record("capability_drop", cleared, operated ? 0 : errno);
    Fd fd{::open("/dev/null", O_RDONLY)};
    const bool operated_range = fd.value >= 0 &&
        ::syscall(SYS_close_range, static_cast<unsigned>(fd.value), static_cast<unsigned>(fd.value), 4U /* CLOSE_RANGE_CLOEXEC */) == 0;
    const int flags = operated_range ? ::fcntl(fd.value, F_GETFD) : -1;
    const bool closed_on_exec = operated_range && flags >= 0 && (flags & FD_CLOEXEC) != 0;
    record("close_range_cloexec", closed_on_exec, closed_on_exec ? 0 : errno);
}
void files() {
    char name[] = "/tmp/ansa-preflight-XXXXXX";
    const auto path = ::mkdtemp(name);
    if (!path) { const int error = errno; record("rename_noreplace", false, error); record("directory_fsync", false, error); return; }
    Fd directory{::open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC)};
    auto make = [&](const char* file, char value) {
        Fd fd{::openat(directory.value, file, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0600)};
        return fd.value >= 0 && ::write(fd.value, &value, 1) == 1 && ::fsync(fd.value) == 0;
    };
    auto read = [&](const char* file, char value) {
        Fd fd{::openat(directory.value, file, O_RDONLY | O_NOFOLLOW | O_CLOEXEC)};
        char actual = 0; return fd.value >= 0 && ::read(fd.value, &actual, 1) == 1 && actual == value;
    };
    const bool created = directory.value >= 0 && make("source", 'a') && make("existing", 'b');
    int rename_error = created ? 0 : errno;
    bool renamed = false;
    if (created) {
        const auto refused = ::syscall(SYS_renameat2, directory.value, "source", directory.value, "existing", RENAME_NOREPLACE);
        const int error = errno;
        if (refused == -1 && error == EEXIST && read("source", 'a') && read("existing", 'b')) {
            renamed = ::syscall(SYS_renameat2, directory.value, "source", directory.value, "published", RENAME_NOREPLACE) == 0 && read("published", 'a');
            rename_error = renamed ? 0 : errno;
        } else rename_error = refused < 0 ? error : 0;
    }
    record("rename_noreplace", renamed, rename_error, "existing_file_not_preserved");
    const bool synced = directory.value >= 0 && ::fsync(directory.value) == 0;
    record("directory_fsync", synced, synced ? 0 : errno);
    for (const char* file : {"source", "existing", "published"}) (void)::unlinkat(directory.value, file, 0);
    directory.reset();
    const bool removed = ::rmdir(path) == 0;
    record("owned_file_cleanup", removed, removed ? 0 : errno);
}
} // namespace

int main(int argc, char**) {
    if (argc != 1) { std::cerr << "usage: ansa_preflight\n"; return 2; }
    struct sigaction defaults{}; defaults.sa_handler = SIG_DFL; ::sigemptyset(&defaults.sa_mask);
    sigset_t unblocked; ::sigemptyset(&unblocked);
    const bool signals = ::sigaction(SIGCHLD, &defaults, nullptr) == 0 && ::sigprocmask(SIG_SETMASK, &unblocked, nullptr) == 0;
    record("child_signal_setup", signals, signals ? 0 : errno);
    // Irreversible flags/capability changes affect this test process and its own children only.
    restrictions(); proc_and_wait(); subreaper(); files();
    bool passed = true;
    std::cout << "{\"schema_version\":1,\"checks\":[";
    for (std::size_t i = 0; i < checks.size(); ++i) {
        const auto& check = checks[i];
        if (i) std::cout << ',';
        std::cout << "{\"id\":" << std::quoted(check.id) << ",\"status\":" << std::quoted(check.status)
                  << ",\"detail\":" << std::quoted(check.detail) << '}';
        passed = passed && check.status == "pass";
    }
    std::cout << "]}\n";
    return passed ? 0 : 1;
}
