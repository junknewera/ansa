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
#include "terminal.hpp"

namespace omh {
namespace {
volatile sig_atomic_t interrupt_fd = -1;
extern "C" void interrupted(int signal) {
    const int saved = errno;
    if (interrupt_fd >= 0) { const char byte = static_cast<char>(signal); const auto count = ::write(interrupt_fd, &byte, 1); (void)count; }
    errno = saved;
}
class Interrupts {
    local::Fd read_, write_;
    struct sigaction previous_[6]{}, pipe_previous_{};
    static constexpr int signals_[6] = {SIGINT, SIGTERM, SIGHUP, SIGTSTP, SIGCONT, SIGWINCH};
public:
    Interrupts() {
        int values[2]; require(::pipe2(values, O_CLOEXEC | O_NONBLOCK) == 0, "cli_signal_pipe", Stage::frontend);
        read_.reset(values[0]); write_.reset(values[1]);
        struct sigaction action{}; action.sa_handler = interrupted; ::sigemptyset(&action.sa_mask);
        for (std::size_t i = 0; i < 6; ++i) {
            if (::sigaction(signals_[i], &action, &previous_[i]) != 0) {
                for (std::size_t j = 0; j < i; ++j) (void)::sigaction(signals_[j], &previous_[j], nullptr);
                fail(ErrorCode::contract, Stage::frontend, "cli_signal_handler");
            }
        }
        interrupt_fd = write_.get();
        action.sa_handler = SIG_IGN;
        if (::sigaction(SIGPIPE, &action, &pipe_previous_) != 0) {
            interrupt_fd = -1; for (std::size_t i = 0; i < 6; ++i) (void)::sigaction(signals_[i], &previous_[i], nullptr);
            fail(ErrorCode::contract, Stage::frontend, "cli_pipe_handler");
        }
    }
    ~Interrupts() {
        interrupt_fd = -1;
        for (std::size_t i = 0; i < 6; ++i) (void)::sigaction(signals_[i], &previous_[i], nullptr);
        (void)::sigaction(SIGPIPE, &pipe_previous_, nullptr);
    }
    int fd() const { return read_.get(); }
    int take() { unsigned char byte; return ::read(read_.get(), &byte, 1) == 1 ? byte : 0; }
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
bool history_page_current(const TranscriptPage& page, const GetTranscript& query, std::size_t known_total) {
    return page.query == query && page.total >= known_total;
}
bool preview_visible(const std::vector<TranscriptEntry>& entries, std::size_t end, std::size_t total,
                     const std::optional<RequestId>& request, const std::optional<PreviewText>& preview) {
    return end == total && !entries.empty() && entries.back().role == "user" && request &&
        entries.back().request == *request && preview && !preview->text.empty() && preview->state != "committed";
}
int text_frontend(Runtime& runtime, const SessionId& session, const std::optional<std::string>& prompt) {
    Interrupts interrupts;
    std::atomic<bool> abort = false, output_failed = false, ended = false;
    Output output(STDOUT_FILENO, abort), diagnostics(STDERR_FILENO, abort);
    const bool terminal = !prompt && ::isatty(STDIN_FILENO) != 0 && ::isatty(STDOUT_FILENO) != 0;
    std::mutex mutex;
    FrontendSnapshot current{};
    TurnId current_turn{"idle"};
    bool initialized = false, transcript_initialized = false;
    std::vector<TranscriptEntry> transcript;
    std::size_t transcript_begin = 0, transcript_end = 0, transcript_total = 0;
    GetTranscript history_query{0, 16, false};
    std::string terminal_status;
    std::optional<PreviewText> preview;
    std::optional<RequestId> preview_request;
    std::unique_ptr<Terminal> screen;
    if (terminal) screen = std::make_unique<Terminal>([&](std::string_view frame) { return output.write(frame); });
    int wake_values[2]; require(::pipe2(wake_values, O_CLOEXEC | O_NONBLOCK) == 0, "cli_event_pipe", Stage::frontend);
    local::Fd wake_read(wake_values[0]), wake_write(wake_values[1]);
    std::atomic<bool> resync_pending = false;
    auto wake = [&] { const char byte = 'W'; const auto count = ::write(wake_write.get(), &byte, 1); (void)count; };
    auto desired_history = [&] { std::lock_guard lock(mutex); return history_query; };
    auto set_history = [&](GetTranscript query) { std::lock_guard lock(mutex); history_query = query; };
    auto report = [&](std::string text) {
        if (!terminal) diagnostics.status(text);
        else { { std::lock_guard lock(mutex); terminal_status = std::move(text); } wake(); }
    };
    std::jthread events([&] {
        try {
            std::optional<RequestId> printed;
            bool first = true;
            while (auto event = runtime.next_event()) {
                if (const auto* view = std::get_if<FrontendSnapshot>(&event->payload)) {
                    const bool fallback = !first && view->state == "completed" && !view->text.empty() && printed != view->request;
                    if (first && view->state == "completed") printed = view->request;
                    first = false;
                    if (fallback && !terminal) {
                        if (!output.write(view->text + "\n")) { output_failed = true; abort = true; runtime.shutdown(); break; }
                        printed = view->request;
                    }
                    // Publish completion only after its text has drained. The input
                    // loop must keep handling interrupts while stdout is blocked.
                    { std::lock_guard lock(mutex); current = *view; current_turn = event->turn; initialized = true; preview = view->preview; preview_request = view->request; }
                    wake();
                } else if (const auto* text = std::get_if<TextDelta>(&event->payload)) {
                    if (!terminal && !output.write(text->text + "\n")) { output_failed = true; abort = true; runtime.shutdown(); break; }
                    printed = event->request;
                    if (terminal) wake();
                } else if (const auto* page = std::get_if<TranscriptPage>(&event->payload)) {
                    { std::lock_guard lock(mutex);
                      const auto query = history_query;
                      if (!history_page_current(*page, query, current.transcript_size)) continue;
                      transcript = page->entries;
                      transcript_initialized = true;
                      transcript_begin = page->begin; transcript_end = page->end; transcript_total = page->total; }
                    wake();
                } else if (const auto* update = std::get_if<PreviewText>(&event->payload)) {
                    { std::lock_guard lock(mutex); preview = *update; preview_request = event->request; }
                    wake();
                } else if (const auto* started = std::get_if<ToolStarted>(&event->payload)) {
                    if (terminal) report("tool: " + (started->name.empty() ? "running" : started->name));
                } else if (const auto* error = std::get_if<RuntimeError>(&event->payload)) {
                    report("ansa: " + error->diagnostic + "\n");
                } else if (std::holds_alternative<ResyncRequired>(event->payload)) {
                    resync_pending = true; wake();
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
    auto next_transcript_retry = std::chrono::steady_clock::time_point::min();
    auto next_resync = std::chrono::steady_clock::time_point::min();
    bool prompted = false;
    int exit_code = 0;
    std::string line;
    std::size_t cursor = 0, scroll = 0, scroll_limit = 0;
    bool expanded = false, mode_initialized = false;
    AgentMode mode = AgentMode::basic;
    std::string escape;
    bool too_long = false;
    auto input = [&](std::string text) {
        FrontendSnapshot view;
        TurnId turn;
        { std::lock_guard lock(mutex); view = current; turn = current_turn; }
        if (text == "/exit" || text == "/quit") { abort = true; runtime.shutdown(); finished = true; return true; }
        if (text == "/basic" || text == "/plan") {
            if ((!pending_submit || view.submit == pending_submit) && !view.worker_busy &&
                (view.state == "idle" || view.state == "completed" || view.state == "cancelled"))
                mode = text == "/plan" ? AgentMode::plan : AgentMode::basic;
            return true;
        }
        if (text == "/status") { report("ansa: " + view.state + " tools=" + std::to_string(view.completed_tools) + "\n"); return true; }
        const bool awaiting = pending_submit && view.submit != pending_submit;
        if (text == "/cancel") {
            if (awaiting) { deferred_cancel = true; (void)send(GetSnapshot{0}, turn); }
            else if (view.request) (void)send(Cancel{*view.request}, turn);
            else report("ansa: no_active_request\n");
            return true;
        }
        if (text == "/resume") {
            if (view.resume_available && view.request) { (void)send(Resume{*view.request}, turn); submitted = true; }
            else report("ansa: resume_not_available\n");
            return true;
        }
        if (text.empty()) return true;
        if (view.state == "paused") { report("ansa: paused; use /resume, /cancel or a new session\n"); exit_code = 1; return false; }
        if (awaiting || view.worker_busy || (view.state != "idle" && view.state != "completed" && view.state != "cancelled")) {
            report("ansa: busy; wait or use Ctrl+C\n"); exit_code = 1; return false;
        }
        const auto id = "t_" + prefix + "_" + std::to_string(sequence + 1);
        submitted = send(Submit{std::move(text), mode}, {id});
        if (submitted) pending_submit = CommandId{"c_" + prefix + "_" + std::to_string(sequence)};
        else { report("ansa: submit_rejected\n"); exit_code = 1; }
        return submitted;
    };
    try {
        while (!finished && !ended.load() && !abort.load()) {
            bool ready, history_ready;
            FrontendSnapshot view;
            TurnId observed_turn{"idle"};
            { std::lock_guard lock(mutex); ready = initialized; history_ready = transcript_initialized;
              view = current; observed_turn = current_turn; }
            if (std::chrono::steady_clock::now() >= next_resync && resync_pending.exchange(false)) {
                if (!send(GetSnapshot{0}, observed_turn)) resync_pending = true;
                next_resync = std::chrono::steady_clock::now() + std::chrono::milliseconds(500);
                next_transcript_retry = next_resync + std::chrono::milliseconds(250);
            }
            if (terminal && ready && !mode_initialized) { mode = view.mode; mode_initialized = true; }
            if (terminal && ready && std::chrono::steady_clock::now() >= next_transcript_retry) {
                (void)send(desired_history(), observed_turn);
                next_transcript_retry = std::chrono::steady_clock::now() + std::chrono::milliseconds(500);
            }
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
            if (terminal && ready && history_ready) {
                std::vector<std::string> lines;
                { std::lock_guard lock(mutex);
                  std::size_t begin = 0, users = 0, candidate = 0;
                  const std::size_t visible_users = !transcript.empty() && transcript.back().role == "user" && transcript.back().state != "completed" ? 4 : 3;
                  if (!expanded) for (std::size_t i = transcript.size(); i > 0; --i) {
                      if (transcript[i - 1].role == "user") {
                          if (++users == visible_users + 1) { begin = candidate; break; }
                          candidate = i - 1;
                      }
                  }
                  for (auto i = begin; i < transcript.size(); ++i) {
                      std::string text = transcript[i].role + ": " + transcript[i].text;
                      std::size_t at = 0, end;
                      while ((end = text.find('\n', at)) != std::string::npos) { lines.push_back(text.substr(at, end - at)); at = end + 1; }
                      lines.push_back(text.substr(at));
                  }
                  if (preview_visible(transcript, transcript_end, transcript_total, preview_request, preview))
                      lines.push_back("assistant: " + preview->text + (preview->state == "discarded" ? " (незавершённый ответ)" : ""));
                  if (!terminal_status.empty()) lines.push_back(terminal_status);
                }
                const auto status = std::string(mode == AgentMode::plan ? "plan" : "basic") + "  " + view.state;
                scroll_limit = screen->render(lines, line, cursor, status, expanded, scroll);
            }
            pollfd waiting[]{{!prompt && ready && (!terminal || history_ready) && !eof ? STDIN_FILENO : -1, POLLIN | POLLHUP, 0},
                {interrupts.fd(), POLLIN, 0}, {wake_read.get(), POLLIN, 0}};
            const auto count = ::poll(waiting, 3, deferred_cancel ? 20 : 100);
            if (count < 0 && errno != EINTR) fail(ErrorCode::contract, Stage::frontend, "cli_input_poll");
            const auto signal = interrupts.take();
            if (signal == SIGTERM || signal == SIGHUP) { abort = true; runtime.shutdown(); exit_code = 128 + signal; break; }
            if (signal == SIGTSTP && screen) {
                screen->restore();
                struct sigaction action{}; action.sa_handler = SIG_DFL; ::sigemptyset(&action.sa_mask);
                (void)::sigaction(SIGTSTP, &action, nullptr); (void)::raise(SIGTSTP);
                action.sa_handler = interrupted; (void)::sigaction(SIGTSTP, &action, nullptr);
                screen->resume();
            }
            if (signal == SIGINT) {
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
                    if (terminal) {
                        const unsigned char ch = static_cast<unsigned char>(bytes[index]);
                        // A standalone Escape must not consume the next ordinary characters.
                        if (escape == "\033" && ch != '[' && ch != 27) escape.clear();
                        if (!escape.empty() || ch == 27) {
                            escape += static_cast<char>(ch);
                            if (escape == "\033[D") { if (cursor) { --cursor; while (cursor && (static_cast<unsigned char>(line[cursor]) & 0xc0) == 0x80) --cursor; } escape.clear(); }
                            else if (escape == "\033[C") { if (cursor < line.size()) { ++cursor; while (cursor < line.size() && (static_cast<unsigned char>(line[cursor]) & 0xc0) == 0x80) ++cursor; } escape.clear(); }
                            else if (escape == "\033[5~") { if (expanded) {
                                std::size_t before; { std::lock_guard lock(mutex); before = transcript_begin; }
                                if (scroll >= scroll_limit && before) { set_history(GetTranscript{before, 16, false}); (void)send(desired_history(), observed_turn); scroll = 0; }
                                else scroll = std::min(scroll + 8, scroll_limit);
                            } escape.clear(); }
                            else if (escape == "\033[6~") {
                                std::size_t end, total; { std::lock_guard lock(mutex); end = transcript_end; total = transcript_total; }
                                if (expanded && scroll == 0 && end < total) { set_history(GetTranscript{end, 16, true}); (void)send(desired_history(), observed_turn); }
                                else scroll = scroll > 8 ? scroll - 8 : 0;
                                escape.clear();
                            }
                            else if (escape == "\033[Z") { if ((!pending_submit || view.submit == pending_submit) && !view.worker_busy && (view.state == "idle" || view.state == "completed" || view.state == "cancelled")) mode = mode == AgentMode::basic ? AgentMode::plan : AgentMode::basic; escape.clear(); }
                            else if (escape.size() > 5 || (escape.size() >= 3 && escape[2] != '5' && escape[2] != '6' && escape[2] != '[')) escape.clear();
                            continue;
                        }
                        if (ch == 6) { expanded = !expanded; scroll = 0; if (!expanded) { set_history(GetTranscript{0, 16, false}); (void)send(desired_history(), observed_turn); } continue; }
                        if (ch == 9) { if ((!pending_submit || view.submit == pending_submit) && !view.worker_busy && (view.state == "idle" || view.state == "completed" || view.state == "cancelled")) mode = mode == AgentMode::basic ? AgentMode::plan : AgentMode::basic; continue; }
                        if (ch == 4) { if (line.empty()) { eof = true; break; } continue; }
                        if (ch == 127 || ch == 8) { if (!too_long && cursor) { auto previous = cursor - 1; while (previous && (static_cast<unsigned char>(line[previous]) & 0xc0) == 0x80) --previous; line.erase(previous, cursor - previous); cursor = previous; } continue; }
                        if (ch == '\r' || ch == '\n') {
                            if (too_long) { report("ansa: submit_size\n"); exit_code = 1; line.clear(); cursor = 0; too_long = false; }
                            else if (input(line)) { line.clear(); cursor = 0; scroll = 0; }
                            if (finished) break; continue;
                        }
                        if (ch >= 32) { if (!too_long && line.size() < max_submit_bytes) { line.insert(cursor, 1, static_cast<char>(ch)); ++cursor; } else too_long = true; }
                        continue;
                    }
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
    abort = true;
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
    StreamingGenerate stream_generate;
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
        stream_generate = [auth, authorization](const Request& request, std::stop_token stop, TextPreview preview) {
            return auth->generate(request, stop, authorization->lease.session, std::move(preview));
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
        {selected.directory, compact, {}, binding, 2, guard, tool_guard, stream_generate});
    std::cerr << "ansa session " << selected.id.value << " (full access, sudo disabled)\n";
    return text_frontend(runtime, selected.id, options.prompt);
}
}
