// Exercise the production frontend with deterministic provider/storage barriers.
#include "../src/agent_cli.cpp"
#include "omh/session.hpp"
#include <sys/wait.h>
#include <termios.h>

namespace {
using namespace omh;
using namespace std::chrono_literals;
void check(bool value, const char* text) { if (!value) throw std::runtime_error(text); }
CompletedResponse final_response(const Request& request) {
    return load_response(Json::object({{"request", save_request(request)}, {"id", Json::string("reply")},
        {"model", Json::string(request.binding().returned_model)}, {"items", Json::array({Json::parse(
        R"({"type":"message","id":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"done"}]})")})}}));
}
void frontend(std::string_view suite) {
    char name[] = "/tmp/ansa-cli-XXXXXX";
    const auto directory = ::mkdtemp(name); check(directory, "mkdtemp");
    const std::filesystem::path root(directory);
    struct Cleanup { std::filesystem::path path; ~Cleanup() { std::error_code error; std::filesystem::remove_all(path, error); } } cleanup{root};
    int input[2], output[2], barrier[2];
    check(::pipe2(input, O_CLOEXEC) == 0 && ::pipe2(output, O_CLOEXEC | O_NONBLOCK) == 0 &&
          ::pipe2(barrier, O_CLOEXEC) == 0, "pipes");
    const bool backpressure = suite == "backpressure";
    if (backpressure) {
        char bytes[4096]{}; while (::write(output[1], bytes, sizeof(bytes)) > 0) {}
    }
    const auto child = ::fork(); check(child >= 0, "fork");
    if (!child) {
        (void)::dup2(input[0], STDIN_FILENO);
        const int null = ::open("/dev/null", O_RDWR);
        (void)::dup2(backpressure ? output[1] : null, STDOUT_FILENO); (void)::dup2(null, STDERR_FILENO);
        ::close(input[0]); ::close(input[1]); ::close(output[0]); ::close(output[1]); ::close(barrier[0]); ::close(null);
        ToolRegistry registry;
        std::atomic<unsigned> generations = 0;
        JournalOptions fault;
        fault.observe = [&](JournalPoint point, std::string_view kind) {
            if (point == JournalPoint::before_sync && kind == "accepted") std::this_thread::sleep_for(400ms);
            if (point == JournalPoint::after_sync && kind == "done") local::write_all(barrier[1], "D");
        };
        Runtime runtime({"cli"}, {"cli", "test", Json::parse("{}")}, registry.snapshot(),
            [&](const Request& request, std::stop_token stop) -> Result<CompletedResponse> {
                ++generations;
                if (suite == "pending-cancel") {
                    for (unsigned n = 0; n < 500 && !stop.stop_requested(); ++n) std::this_thread::sleep_for(10ms);
                    if (stop.stop_requested()) return Error{ErrorCode::cancelled, Stage::admission, "cancelled"};
                }
                if (suite == "busy") std::this_thread::sleep_for(200ms);
                return final_response(request);
            }, {.events = backpressure ? 2U : 64U}, {root / "session", {}, fault});
        const auto result = text_frontend(runtime, {"cli"}, backpressure ? std::optional<std::string>{"task"} : std::nullopt);
        if (suite == "pending-cancel" && generations != 0) ::_exit(20);
        ::_exit(result);
    }
    ::close(input[0]); ::close(output[1]); ::close(barrier[1]);
    if (suite == "pending-cancel") local::write_all(input[1], "task\n/cancel\n");
    else if (suite == "busy") local::write_all(input[1], "task\nsecond task\n");
    else if (suite == "input-size") local::write_all(input[1], std::string(max_submit_bytes + 1, 'x'));
    ::close(input[1]);
    if (backpressure) {
        pollfd ready{barrier[0], POLLIN, 0};
        check(::poll(&ready, 1, 3000) > 0 && (ready.revents & POLLIN), "completion barrier");
        // Allow the completed snapshot to reach the full output pipe.
        std::this_thread::sleep_for(100ms);
        (void)::kill(child, SIGINT);
    }
    int status = 0; pid_t reaped = 0;
    const auto deadline = std::chrono::steady_clock::now() + 3s;
    while (!(reaped = ::waitpid(child, &status, WNOHANG)) && std::chrono::steady_clock::now() < deadline)
        std::this_thread::sleep_for(5ms);
    if (!reaped) { (void)::kill(child, SIGKILL); (void)::waitpid(child, &status, 0); }
    ::close(output[0]); ::close(barrier[0]);
    check(reaped == child && WIFEXITED(status), "frontend failed to stop with blocked output or pending cancellation");
    check(WEXITSTATUS(status) == (backpressure ? 130 : (suite == "busy" || suite == "input-size") ? 1 : 0),
          "frontend reported wrong acceptance status");
}
void terminal_frontend() {
    const GetTranscript latest{0, 16, false}, forward{10, 16, true};
    const TranscriptPage page{10, 12, 12, {{{"turn"}, {"request"}, "user", "task", "pending"}}, forward};
    check(history_page_current(page, forward, 12), "forward page correlation");
    check(!history_page_current(page, latest, 12), "stale page after collapse");
    check(!history_page_current(page, forward, 13), "stale transcript total");
    const std::optional<RequestId> request = RequestId{"request"};
    const std::optional<PreviewText> draft = PreviewText{1, "draft", "discarded"};
    check(preview_visible(page.entries, page.end, page.total, request, draft), "current preview");
    check(!preview_visible(page.entries, 10, 12, request, draft), "preview on older page");
    check(!preview_visible(page.entries, page.end, page.total, RequestId{"other"}, draft), "preview request identity");
    char name[] = "/tmp/ansa-pty-XXXXXX";
    const auto directory = ::mkdtemp(name); check(directory, "mkdtemp");
    struct Cleanup { std::filesystem::path path; ~Cleanup() { std::error_code error; std::filesystem::remove_all(path, error); } } cleanup{directory};
    const int master = ::posix_openpt(O_RDWR | O_NOCTTY | O_NONBLOCK);
    check(master >= 0 && ::grantpt(master) == 0 && ::unlockpt(master) == 0, "pty");
    const int slave = ::open(::ptsname(master), O_RDWR | O_NOCTTY); check(slave >= 0, "pty slave");
    termios original{}; check(::tcgetattr(slave, &original) == 0, "pty termios");
    const auto child = ::fork(); check(child >= 0, "fork");
    if (!child) {
        (void)::setenv("LC_ALL", "C", 1); (void)::unsetenv("LOCALE_ARCHIVE");
        (void)::dup2(slave, STDIN_FILENO); (void)::dup2(slave, STDOUT_FILENO);
        const int null = ::open("/dev/null", O_WRONLY); (void)::dup2(null, STDERR_FILENO);
        ::close(master); ::close(slave); ::close(null);
        ToolRegistry registry;
        SessionOptions options; options.directory = std::filesystem::path(directory) / "session";
        {
            Runtime seed({"cli"}, {"cli", "test", Json::parse("{}")}, registry.snapshot(),
                [](const Request& request, std::stop_token) -> Result<CompletedResponse> { return final_response(request); },
                {}, {options.directory});
            const auto accepted = seed.post({1, {"cli"}, {"previous"}, seed.incarnation(), {"previous"}, Submit{"EARLIER_CONTEXT"}});
            check(std::holds_alternative<PostStatus>(accepted) && std::get<PostStatus>(accepted) == PostStatus::accepted, "seed submit");
            while (auto event = seed.next_event()) if (std::holds_alternative<Done>(event->payload)) break;
            seed.close_input(); while (seed.next_event()) {}
        }
        options.stream_generate = [](const Request& request, std::stop_token stop, TextPreview preview) -> Result<CompletedResponse> {
            std::string text = "draft";
            for (int i = 0; i < 300 && !stop.stop_requested(); ++i) { text += 'x'; preview(text); }
            for (int i = 0; i < 300 && !stop.stop_requested(); ++i) std::this_thread::sleep_for(10ms);
            if (stop.stop_requested()) return Error{ErrorCode::cancelled, Stage::admission, "cancelled"};
            return final_response(request);
        };
        Runtime runtime({"cli"}, {"cli", "test", Json::parse("{}")}, registry.snapshot(),
            [](const Request& request, std::stop_token stop) -> Result<CompletedResponse> {
                for (int i = 0; i < 300 && !stop.stop_requested(); ++i) std::this_thread::sleep_for(10ms);
                if (stop.stop_requested()) return Error{ErrorCode::cancelled, Stage::admission, "cancelled"};
                return final_response(request);
            },
            {.events = 2}, std::move(options));
        ::_exit(text_frontend(runtime, {"cli"}, std::nullopt));
    }
    std::string output;
    auto drain = [&] { char bytes[8192]; ssize_t n; while ((n = ::read(master, bytes, sizeof(bytes))) > 0) output.append(bytes, static_cast<std::size_t>(n)); };
    auto wait_for = [&](std::string_view text) {
        const auto until = std::chrono::steady_clock::now() + 3s;
        while (output.find(text) == std::string::npos && std::chrono::steady_clock::now() < until) {
            pollfd ready{master, POLLIN, 0}; (void)::poll(&ready, 1, 20); drain();
        }
        if (output.find(text) == std::string::npos) throw std::runtime_error("pty expected: " + std::string(text));
    };
    wait_for("basic");
    check(output.find("user: EARLIER_CONTEXT") < output.find("> "), "history must precede first input prompt");
    std::string overflow(max_submit_bytes * 2 + 100, 'x');
    overflow.append(max_submit_bytes, '\177'); overflow += '\r';
    std::size_t sent = 0;
    const auto write_until = std::chrono::steady_clock::now() + 5s;
    while (sent < overflow.size() && std::chrono::steady_clock::now() < write_until) {
        const auto n = ::write(master, overflow.data() + sent, overflow.size() - sent);
        if (n > 0) sent += static_cast<std::size_t>(n);
        else if (errno != EAGAIN && errno != EINTR) break;
        pollfd ready{master, POLLIN | POLLOUT, 0}; (void)::poll(&ready, 1, 10); drain();
    }
    check(sent == overflow.size(), "pty overflow input"); wait_for("submit_size");
    local::write_all(master, "\xc0\xaf"); wait_for("> ??");
    local::write_all(master, "\177");
    local::write_all(master, "e\xcc\x81😀"); wait_for("e\xcc\x81😀");
    wait_for("\033[5C");
    local::write_all(master, "\177\177\177");
    local::write_all(master, "\t"); wait_for("plan");
    local::write_all(master, "\x06\033[5~\033[6~привет\177т"); wait_for("привет");
    local::write_all(master, "\033[Z\r");
    wait_for("user: привет");
    wait_for("assistant: draft");
    local::write_all(master, "next\r"); wait_for("busy"); wait_for("> next");
    winsize narrow{}; narrow.ws_row = 10; narrow.ws_col = 20;
    check(::ioctl(master, TIOCSWINSZ, &narrow) == 0, "pty resize");
    (void)::kill(child, SIGWINCH);
    (void)::kill(child, SIGINT);
    std::this_thread::sleep_for(100ms);
    (void)::kill(child, SIGTERM);
    int status = 0; pid_t reaped = 0; const auto until = std::chrono::steady_clock::now() + 3s;
    while ((reaped = ::waitpid(child, &status, WNOHANG)) == 0 && std::chrono::steady_clock::now() < until) {
        pollfd ready{master, POLLIN, 0}; (void)::poll(&ready, 1, 20); drain();
    }
    if (!reaped) { (void)::kill(child, SIGKILL); (void)::waitpid(child, &status, 0); }
    check(reaped == child && WIFEXITED(status), "terminal process exit");
    termios restored{}; check(::tcgetattr(slave, &restored) == 0, "pty termios after exit");
    check((restored.c_lflag & (ICANON | ECHO)) == (original.c_lflag & (ICANON | ECHO)), "terminal restoration");
    ::close(master); ::close(slave);
}
}
int main(int argc, char** argv) {
    try { check(argc == 2, "suite required"); if (std::string_view(argv[1]) == "terminal") terminal_frontend(); else frontend(argv[1]); return 0; }
    catch (const std::exception& error) { std::cerr << error.what() << '\n'; return 1; }
}
