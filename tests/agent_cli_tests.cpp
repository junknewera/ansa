// Exercise the production frontend with deterministic provider/storage barriers.
#include "../src/agent_cli.cpp"
#include "omh/session.hpp"
#include <sys/wait.h>

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
}
int main(int argc, char** argv) {
    try { check(argc == 2, "suite required"); frontend(argv[1]); return 0; }
    catch (const std::exception& error) { std::cerr << error.what() << '\n'; return 1; }
}
