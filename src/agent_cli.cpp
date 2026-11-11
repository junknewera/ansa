#include "omh/agent_config.hpp"
#include "omh/auth.hpp"
#include "omh/local_storage.hpp"
#include "omh/local_tools.hpp"
#include "omh/offline.hpp"
#include "omh/runtime.hpp"
#include <atomic>
#include <csignal>
#include <cstdlib>
#include <fcntl.h>
#include <iostream>
#include <mutex>
#include <poll.h>
#include <set>
#include <thread>
#include <unistd.h>

namespace omh {
namespace {
volatile sig_atomic_t interrupt_fd = -1;
extern "C" void interrupted(int) {
    const int saved = errno;
    if (interrupt_fd >= 0) { const char byte = 'I'; const auto count = ::write(interrupt_fd, &byte, 1); (void)count; }
    errno = saved;
}
class Interrupts {
    local::Fd read_, write_;
    struct sigaction previous_{}, pipe_previous_{};
public:
    Interrupts() {
        int values[2]; require(::pipe2(values, O_CLOEXEC | O_NONBLOCK) == 0, "cli_signal_pipe", Stage::frontend);
        read_.reset(values[0]); write_.reset(values[1]);
        struct sigaction action{}; action.sa_handler = interrupted; ::sigemptyset(&action.sa_mask);
        require(::sigaction(SIGINT, &action, &previous_) == 0, "cli_signal_handler", Stage::frontend);
        interrupt_fd = write_.get();
        action.sa_handler = SIG_IGN;
        if (::sigaction(SIGPIPE, &action, &pipe_previous_) != 0) {
            interrupt_fd = -1; (void)::sigaction(SIGINT, &previous_, nullptr);
            fail(ErrorCode::contract, Stage::frontend, "cli_pipe_handler");
        }
    }
    ~Interrupts() {
        interrupt_fd = -1;
        (void)::sigaction(SIGINT, &previous_, nullptr); (void)::sigaction(SIGPIPE, &pipe_previous_, nullptr);
    }
    int fd() const { return read_.get(); }
    bool take() { char bytes[64]; return ::read(read_.get(), bytes, sizeof(bytes)) > 0; }
};
class Output {
    int fd_, flags_;
    const std::atomic<bool>& abort_;
public:
    Output(int fd, const std::atomic<bool>& abort) : fd_(fd), flags_(::fcntl(fd, F_GETFL)), abort_(abort) {
        require(flags_ >= 0 && ::fcntl(fd, F_SETFL, flags_ | O_NONBLOCK) == 0, "cli_output", Stage::frontend);
    }
    ~Output() { (void)::fcntl(fd_, F_SETFL, flags_); }
    bool write(std::string_view text) {
        while (!text.empty() && !abort_.load()) {
            const auto count = ::write(fd_, text.data(), text.size());
            if (count > 0) { text.remove_prefix(static_cast<std::size_t>(count)); continue; }
            if (count < 0 && errno == EINTR) continue;
            if (count < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
                pollfd ready{fd_, POLLOUT, 0}; (void)::poll(&ready, 1, 50); continue;
            }
            return false;
        }
        return text.empty();
    }
    void status(std::string_view text) const {
        // Progress must never block the input/control thread on a full stderr.
        const auto count = ::write(fd_, text.data(), text.size()); (void)count;
    }
};
bool post(Runtime& runtime, RuntimeCommand command) {
    const auto value = runtime.post(std::move(command));
    if (const auto* error = std::get_if<Error>(&value)) throw *error;
    const auto status = std::get<PostStatus>(value);
    return status == PostStatus::accepted || status == PostStatus::duplicate;
}
int text_frontend(Runtime& runtime, const SessionId& session, const std::optional<std::string>& prompt) {
    Interrupts interrupts;
    std::atomic<bool> abort = false, output_failed = false, ended = false;
    Output output(STDOUT_FILENO, abort), diagnostics(STDERR_FILENO, abort);
    const bool terminal = ::isatty(STDIN_FILENO) != 0;
    std::mutex mutex;
    FrontendSnapshot current{};
    TurnId current_turn{"idle"};
    bool initialized = false;
    int wake_values[2]; require(::pipe2(wake_values, O_CLOEXEC | O_NONBLOCK) == 0, "cli_event_pipe", Stage::frontend);
    local::Fd wake_read(wake_values[0]), wake_write(wake_values[1]);
    auto wake = [&] { const char byte = 'W'; const auto count = ::write(wake_write.get(), &byte, 1); (void)count; };
    std::jthread events([&] {
        try {
            std::optional<RequestId> printed;
            bool first = true;
            while (auto event = runtime.next_event()) {
                if (const auto* view = std::get_if<FrontendSnapshot>(&event->payload)) {
                    const bool fallback = !first && view->state == "completed" && !view->text.empty() && printed != view->request;
                    if (first && view->state == "completed") printed = view->request;
                    first = false;
                    if (fallback) {
                        if (!output.write(view->text + "\n")) { output_failed = true; abort = true; runtime.shutdown(); break; }
                        printed = view->request;
                    }
                    // Publish completion only after its text has drained. The input
                    // loop must keep handling interrupts while stdout is blocked.
                    { std::lock_guard lock(mutex); current = *view; current_turn = event->turn; initialized = true; }
                    wake();
                } else if (const auto* text = std::get_if<TextDelta>(&event->payload)) {
                    if (!output.write(text->text + "\n")) { output_failed = true; abort = true; runtime.shutdown(); break; }
                    printed = event->request;
                } else if (const auto* started = std::get_if<ToolStarted>(&event->payload)) {
                    if (terminal) diagnostics.status("tool: " + (started->name.empty() ? "running" : started->name) + "\n");
                } else if (const auto* error = std::get_if<RuntimeError>(&event->payload)) {
                    diagnostics.status("ansa: " + error->diagnostic + "\n");
                } else if (std::holds_alternative<ResyncRequired>(event->payload)) {
                    (void)post(runtime, {1, session, event->turn, runtime.incarnation(), {"resync"}, GetSnapshot{0}});
                }
            }
        } catch (...) { output_failed = true; abort = true; runtime.shutdown(); }
        ended = true; wake();
    });
    std::size_t sequence = 0;
    const auto prefix = runtime.incarnation().value.substr(0, 12);
    auto send = [&](CommandPayload payload, TurnId turn) {
        return post(runtime, {1, session, std::move(turn), runtime.incarnation(),
            {"c_" + prefix + "_" + std::to_string(++sequence)}, std::move(payload)});
    };
    bool submitted = false, eof = false, finished = false, cancel_requested = false;
    std::optional<CommandId> pending_submit;
    bool deferred_cancel = false, input_closed = false;
    auto next_pending_snapshot = std::chrono::steady_clock::time_point::min();
    bool prompted = false;
    int exit_code = 0;
    std::string line;
    bool too_long = false;
    auto input = [&](std::string text) {
        FrontendSnapshot view;
        TurnId turn;
        { std::lock_guard lock(mutex); view = current; turn = current_turn; }
        if (text == "/exit" || text == "/quit") { abort = true; runtime.shutdown(); finished = true; return; }
        if (text == "/status") { diagnostics.status("ansa: " + view.state + " tools=" + std::to_string(view.completed_tools) + "\n"); return; }
        const bool awaiting = pending_submit && view.submit != pending_submit;
        if (text == "/cancel") {
            if (awaiting) { deferred_cancel = true; (void)send(GetSnapshot{0}, turn); }
            else if (view.request) (void)send(Cancel{*view.request}, turn);
            else diagnostics.status("ansa: no_active_request\n");
            return;
        }
        if (text == "/resume") {
            if (view.resume_available && view.request) { (void)send(Resume{*view.request}, turn); submitted = true; }
            else diagnostics.status("ansa: resume_not_available\n");
            return;
        }
        if (text.empty()) return;
        if (view.state == "paused") { diagnostics.status("ansa: paused; use /resume, /cancel or a new session\n"); exit_code = 1; return; }
        if (awaiting || view.worker_busy || (view.state != "idle" && view.state != "completed" && view.state != "cancelled")) {
            diagnostics.status("ansa: busy; wait or use Ctrl+C\n"); exit_code = 1; return;
        }
        const auto id = "t_" + prefix + "_" + std::to_string(sequence + 1);
        submitted = send(Submit{std::move(text)}, {id});
        if (submitted) pending_submit = CommandId{"c_" + prefix + "_" + std::to_string(sequence)};
        else { diagnostics.status("ansa: submit_rejected\n"); exit_code = 1; }
    };
    try {
        while (!finished && !ended.load() && !abort.load()) {
            bool ready;
            FrontendSnapshot view;
            { std::lock_guard lock(mutex); ready = initialized; view = current; }
            if (deferred_cancel && pending_submit && (view.submit == pending_submit || view.storage_failed)) {
                TurnId turn;
                { std::lock_guard lock(mutex); turn = current_turn; }
                if (view.request) (void)send(Cancel{*view.request}, turn);
                deferred_cancel = false;
            }
            if (deferred_cancel && std::chrono::steady_clock::now() >= next_pending_snapshot) {
                TurnId turn;
                { std::lock_guard lock(mutex); turn = current_turn; }
                (void)send(GetSnapshot{0}, turn);
                next_pending_snapshot = std::chrono::steady_clock::now() + std::chrono::milliseconds(40);
            }
            if (eof && !input_closed && !deferred_cancel) { runtime.close_input(); input_closed = true; }
            if (ready && prompt && !submitted) { input(*prompt); if (!submitted) { exit_code = 1; break; } }
            if (ready && prompt && submitted && (view.state == "completed" || view.state == "cancelled" || view.state == "paused")) {
                std::lock_guard lock(mutex);
                if (current.submit && current.submit->value.starts_with("c_" + prefix)) {
                    if (exit_code != 130) exit_code = current.state == "completed" ? 0 : 1;
                    break;
                }
            }
            if (ready && terminal && !prompt && !prompted && !view.worker_busy) {
                diagnostics.status("ansa> "); prompted = true;
            }
            pollfd waiting[]{{!prompt && ready && !eof ? STDIN_FILENO : -1, POLLIN | POLLHUP, 0},
                {interrupts.fd(), POLLIN, 0}, {wake_read.get(), POLLIN, 0}};
            const auto count = ::poll(waiting, 3, deferred_cancel ? 20 : 100);
            if (count < 0 && errno != EINTR) fail(ErrorCode::contract, Stage::frontend, "cli_input_poll");
            if (interrupts.take()) {
                if (prompt) exit_code = 130;
                TurnId turn;
                { std::lock_guard lock(mutex); view = current; turn = current_turn; }
                if (!cancel_requested && pending_submit && view.submit != pending_submit) {
                    deferred_cancel = true; cancel_requested = true;
                    (void)send(GetSnapshot{0}, turn);
                } else if (!cancel_requested && view.request && view.state != "completed" && view.state != "cancelled" && view.state != "idle" &&
                    send(Cancel{*view.request}, turn)) {
                    cancel_requested = true;
                } else { abort = true; runtime.shutdown(); break; }
            }
            if (waiting[2].revents) {
                char bytes[256]; while (::read(wake_read.get(), bytes, sizeof(bytes)) > 0) {}
                prompted = false;
                if (!view.worker_busy && (view.state == "completed" || view.state == "cancelled")) cancel_requested = false;
            }
            if (!prompt && ready && waiting[0].revents) {
                char bytes[4096]; const auto size = ::read(STDIN_FILENO, bytes, sizeof(bytes));
                if (size < 0 && errno == EINTR) continue;
                require(size >= 0, "cli_input_read", Stage::frontend);
                if (!size) {
                    if (too_long) { diagnostics.status("ansa: submit_size\n"); exit_code = 1; }
                    if (!line.empty() && !too_long) input(std::move(line));
                    eof = true;
                } else for (ssize_t index = 0; index < size; ++index) {
                    if (bytes[index] == '\n') {
                        if (too_long) { diagnostics.status("ansa: submit_size\n"); exit_code = 1; }
                        else input(std::move(line));
                        line.clear(); too_long = false; prompted = false;
                        if (finished) break;
                    } else if (line.size() < max_submit_bytes) line += bytes[index];
                    else too_long = true;
                }
            }
        }
    } catch (...) { abort = true; runtime.shutdown(); events.join(); throw; }
    if (prompt || finished || abort.load()) runtime.shutdown(); else runtime.close_input();
    events.join();
    if (exit_code == 130) return 130;
    if (current.state == "paused" || current.storage_failed) exit_code = 1;
    return output_failed ? 1 : exit_code;
}
struct Options {
    bool offline = false, resume = false, print_config = false;
    std::filesystem::path cwd = std::filesystem::current_path(), auth = default_auth_directory();
    std::string session;
    std::optional<std::string> model, returned_model, prompt;
};
Options parse(int argc, char** argv) {
    Options options;
    std::set<std::string> seen;
    for (int index = 1; index < argc; ++index) {
        const std::string key = argv[index];
        if (!key.starts_with('-')) {
            require(!options.prompt, "cli_prompt_arguments", Stage::frontend);
            options.prompt = key; continue;
        }
        require(seen.insert(key).second, "duplicate_cli_option", Stage::frontend);
        if (key == "--offline") options.offline = true;
        else if (key == "--continue") options.resume = true;
        else if (key == "--print-config") options.print_config = true;
        else {
            require(index + 1 < argc, "cli_option_value", Stage::frontend);
            const std::string value = argv[++index];
            if (key == "--session") options.session = value;
            else if (key == "--cwd") options.cwd = value;
            else if (key == "--auth-dir") options.auth = value;
            else if (key == "--model") options.model = value;
            else if (key == "--returned-model") options.returned_model = value;
            else fail(ErrorCode::contract, Stage::frontend, "unknown_cli_option");
        }
    }
    if (options.prompt)
        require(*options.prompt != "/resume" && *options.prompt != "/cancel" && *options.prompt != "/status" &&
                *options.prompt != "/exit" && *options.prompt != "/quit", "slash_command_requires_stdin", Stage::frontend);
    return options;
}
}
int agent_cli(int argc, char** argv) {
    const auto options = parse(argc, argv);
    const auto* home = std::getenv("HOME");
    require(home && *home, "home_directory_unavailable", Stage::frontend);
    auto settings = load_agent_settings(options.cwd, home);
    if (options.model) { settings.model = *options.model; settings.returned_model = *options.model; }
    if (options.returned_model) settings.returned_model = *options.returned_model;
    if (options.print_config) { std::cout << settings.effective().dump() << '\n'; return 0; }
    std::shared_ptr<AuthManager> auth;
    struct Authorization { AuthLease lease; std::uint64_t epoch = 0; };
    auto authorization = std::make_shared<Authorization>();
    Binding binding;
    binding.requested_model = settings.model; binding.returned_model = settings.returned_model;
    if (!options.offline) {
        auth = std::make_shared<AuthManager>(options.auth);
        authorization->lease = auth->access();
        binding.account = authorization->lease.account;
        const auto models = auth->models().at("models").elements();
        require(std::any_of(models.begin(), models.end(), [&](const Json& model) {
            return model.text("slug") == settings.model && model.text("visibility") == "list";
        }), "model_not_available", Stage::frontend);
    }
    auto epoch = agent_epoch(settings);
    const auto selected = select_session(settings, options.resume, options.session, options.offline ? "offline" : "live");
    const auto tools = local_tools(settings, epoch, selected.directory, options.auth);
    Generate generate;
    std::function<void(std::uint64_t, std::stop_token)> guard;
    std::function<ToolLease(std::stop_token)> tool_guard;
    if (auth) {
        const auto account = binding.account;
        guard = [auth, authorization, account](std::uint64_t number, std::stop_token stop) {
            const auto current = auth->access(stop);
            require(current.account == account, "auth_account_mismatch", Stage::frontend);
            if (number != authorization->epoch) { authorization->epoch = number; authorization->lease = current; }
            require(current.session == authorization->lease.session, "auth_required", Stage::frontend);
        };
        generate = [auth, authorization](const Request& request, std::stop_token stop) {
            return auth->generate(request, stop, authorization->lease.session);
        };
        tool_guard = [auth, authorization](std::stop_token stop) {
            auto lease = auth->hold_execution(authorization->lease.session, stop);
            return ToolLease{std::move(lease.ownership), lease.stop};
        };
    } else generate = offline_generate;
    const Compact compact = auth ? Compact{[generate](const Request& source, std::stop_token stop) -> Result<CompactedContext> {
        try {
            auto response = generate(summary_request(source), stop);
            if (auto* failure = std::get_if<Error>(&response)) return *failure;
            return CompactedContext::from_summary(source, std::get<CompletedResponse>(response));
        } catch (const Error& failure) { return failure; }
    }} : Compact{offline_compact};
    Runtime runtime(selected.id, epoch, tools, generate,
        {.compact_bytes = settings.compact_bytes, .tool_rounds = settings.tool_rounds, .tool_calls = settings.tool_calls},
        {selected.directory, compact, {}, binding, 2, guard, tool_guard});
    std::cerr << "ansa session " << selected.id.value << " (full access, sudo disabled)\n";
    return text_frontend(runtime, selected.id, options.prompt);
}
}
