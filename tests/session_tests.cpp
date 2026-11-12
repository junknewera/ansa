#include "omh/offline.hpp"
#include "omh/runtime.hpp"
#include "omh/session.hpp"
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <fcntl.h>
#include <fstream>
#include <iostream>
#include <mutex>
#include <poll.h>
#include <signal.h>
#include <stdexcept>
#include <sys/mman.h>
#include <sys/wait.h>
#include <thread>
#include <unistd.h>

namespace {
using namespace omh;
using namespace std::chrono_literals;
void check(bool ok, const char* message) { if (!ok) throw std::runtime_error(message); }
template<class T> T unwrap(Result<T> result) {
    if (const auto* error = std::get_if<Error>(&result)) throw *error;
    return std::get<T>(std::move(result));
}
template<class F> void rejects(F action, std::string_view diagnostic) {
    try { action(); }
    catch (const Error& error) { check(error.diagnostic == diagnostic, "wrong diagnostic"); return; }
    throw std::runtime_error("expected rejection");
}
struct Temp {
    std::filesystem::path root, session;
    Temp() {
        char name[] = "/tmp/omh-session-XXXXXX";
        const auto directory = ::mkdtemp(name);
        check(directory, "mkdtemp");
        root = directory;
        session = root / "session";
    }
    ~Temp() { std::error_code error; std::filesystem::remove_all(root, error); }
};
struct Counters { std::atomic<unsigned> tools{0}, generations{0}, compactions{0}; };
Counters* counts;
class Gate {
    std::mutex mutex_;
    std::condition_variable changed_;
    bool entered_ = false, released_ = false;
public:
    void hold() {
        std::unique_lock lock(mutex_);
        entered_ = true;
        changed_.notify_all();
        check(changed_.wait_for(lock, 5s, [&] { return released_; }), "gate release timeout");
    }
    void wait() {
        std::unique_lock lock(mutex_);
        check(changed_.wait_for(lock, 5s, [&] { return entered_; }), "gate entry timeout");
    }
    void release() {
        std::lock_guard lock(mutex_);
        released_ = true;
        changed_.notify_all();
    }
};
Gate* executor_gate = nullptr;
ToolOutcome counted(const Json& args, std::stop_token stop) {
    ++counts->tools;
    if (executor_gate) executor_gate->hold();
    return test_value_tool().execute(args, stop);
}
std::shared_ptr<const ToolSnapshot> tools() {
    ToolRegistry registry;
    auto definition = test_value_tool();
    definition.execute = counted;
    registry.register_tool(std::move(definition));
    return registry.snapshot();
}
ContextEpoch epoch() { return {"session_epoch", "Use the pure fixture tool", Json::parse(R"({"mode":"offline","revision":1})")}; }
Result<CompletedResponse> generate(const Request& request, std::stop_token stop) {
    ++counts->generations;
    return offline_generate(request, stop);
}
Result<CompactedContext> compact(const Request& request, std::stop_token stop) {
    ++counts->compactions;
    return offline_compact(request, stop);
}
std::unique_ptr<Runtime> runtime(const std::filesystem::path& path, JournalOptions journal = {},
                                 RuntimeLimits limits = {}, Compact compactor = compact, Generate generator = generate) {
    return std::make_unique<Runtime>(SessionId{"offline"}, epoch(), tools(), std::move(generator), limits,
                                    SessionOptions{path, std::move(compactor), std::move(journal)});
}
RuntimeCommand cmd(const Runtime& runtime, std::string id, CommandPayload payload, TurnId turn = {"turn"}) {
    return {1, {"offline"}, std::move(turn), runtime.incarnation(), {std::move(id)}, std::move(payload)};
}
void post(Runtime& runtime, RuntimeCommand command, PostStatus expected = PostStatus::accepted) {
    const auto status = unwrap(runtime.post(std::move(command)));
    check(status == expected, "unexpected post status");
}
template<class Payload, class Match>
RuntimeEvent until(Runtime& runtime, Match match) {
    while (auto event = runtime.next_event()) {
        validate(*event);
        if (const auto* payload = std::get_if<Payload>(&event->payload); payload && match(*payload)) return *event;
        if constexpr (!std::is_same_v<Payload, RuntimeError>) {
            if (const auto* error = std::get_if<RuntimeError>(&event->payload))
                throw std::runtime_error(error->diagnostic);
        }
    }
    throw std::runtime_error("missing event");
}
template<class Payload> RuntimeEvent until(Runtime& runtime) {
    return until<Payload>(runtime, [](const auto&) { return true; });
}
FrontendSnapshot snapshot(Runtime& runtime, const std::string& id = "view") {
    post(runtime, cmd(runtime, id, GetSnapshot{0}));
    return std::get<FrontendSnapshot>(until<FrontendSnapshot>(runtime,
        [&](const FrontendSnapshot& value) { return value.command == CommandId{id}; }).payload);
}
RequestId submit(Runtime& runtime, std::string id = "submit", std::string text = "fixture", TurnId turn = {"turn"}) {
    post(runtime, cmd(runtime, id, Submit{std::move(text)}, std::move(turn)));
    return *std::get<CommandAccepted>(until<CommandAccepted>(runtime).payload).request;
}
void finish(Runtime& runtime) {
    runtime.close_input();
    while (auto event = runtime.next_event()) {
        if (const auto* error = std::get_if<RuntimeError>(&event->payload)) throw std::runtime_error(error->diagnostic);
    }
}
std::string bytes(const std::filesystem::path& path) {
    std::ifstream file(path, std::ios::binary);
    check(bool(file), "file read");
    return {std::istreambuf_iterator<char>(file), {}};
}
std::vector<std::filesystem::path> parts(const std::filesystem::path& path) {
    std::vector<std::filesystem::path> out;
    for (const auto& entry : std::filesystem::directory_iterator(path))
        if (entry.path().filename().string().starts_with("part-")) out.push_back(entry.path());
    std::sort(out.begin(), out.end());
    return out;
}
Json record(unsigned value) {
    return Json::object({{"kind", Json::string("test")}, {"value", Json::parse(std::to_string(value))}});
}

void journal_integrity() {
    Temp temp;
    std::vector<unsigned> values;
    {
        Journal journal(temp.session, [&](const Json&) {});
        journal.append(record(1));
        journal.append(record(2));
        rejects([&] { Journal another(temp.session, [&](const Json&) {}); }, "session_busy");
        const auto child = ::fork();
        check(child >= 0, "lock fork");
        if (child == 0) {
            try { Journal another(temp.session, [&](const Json&) {}); }
            catch (const Error& error) { ::_exit(error.diagnostic == "session_busy" ? 0 : 2); }
            ::_exit(3);
        }
        int status = 0;
        check(::waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0,
              "second process acquired session lock");
    }
    auto files = parts(temp.session);
    check(files.size() == 1, "initial part count");
    const auto size = std::filesystem::file_size(files[0]);
    check(::truncate(files[0].c_str(), static_cast<off_t>(size - 7)) == 0, "truncate fixture");
    const auto torn = bytes(files[0]);
    {
        Journal journal(temp.session, [&](const Json& event) { values.push_back(static_cast<unsigned>(event.index("value"))); });
        check(journal.diagnostic().empty() && values == std::vector<unsigned>{1}, "torn tail recovery");
        journal.append(record(3));
    }
    check(bytes(files[0]) == torn && parts(temp.session).size() == 2, "recovery changed torn bytes");
    // Crash while creating the recovery part header, followed by another restart.
    const auto interrupted = temp.session / "part-00000000000000000002.log";
    { std::ofstream file(interrupted); file << "OMHJ1 "; }
    values.clear();
    {
        Journal journal(temp.session, [&](const Json& event) { values.push_back(static_cast<unsigned>(event.index("value"))); });
        check(journal.diagnostic().empty() && values == std::vector<unsigned>({1, 3}), "partial part header recovery");
        journal.append(record(4));
    }
    values.clear();
    {
        Journal journal(temp.session, [&](const Json& event) { values.push_back(static_cast<unsigned>(event.index("value"))); });
        check(journal.diagnostic().empty() && values == std::vector<unsigned>({1, 3, 4}), "chained recovery");
    }
    {
        std::fstream file(files[0], std::ios::in | std::ios::out | std::ios::binary);
        file.seekp(92 + 83 + 12);
        file.put('X');
    }
    Journal corrupted(temp.session, [&](const Json&) {});
    check(corrupted.diagnostic() == "journal_checksum", "middle corruption accepted");
    rejects([&] { corrupted.append(record(5)); }, "journal_read_only");
}
void journal_conflict() {
    Temp temp;
    Journal journal(temp.session, [&](const Json&) {});
    journal.append(record(1));
    const auto file = parts(temp.session).front();
    { std::ofstream append(file, std::ios::app); append << "external writer"; }
    rejects([&] { journal.append(record(2)); }, "journal_write_conflict");
}

void history_and_dedup() {
    Temp temp;
    RuntimeIncarnation previous;
    RequestId first;
    {
        auto run = runtime(temp.session);
        previous = run->incarnation();
        first = submit(*run);
        until<Done>(*run);
        finish(*run);
    }
    const auto calls = counts->generations.load();
    {
        auto run = runtime(temp.session);
        const auto view = snapshot(*run);
        check(view.state == "completed" && view.durable && view.history_items == 5 && view.tools.size() == 1 &&
              view.text == "OMH_TOOL_OK:OMH_FIXED_VALUE_42", "completed session was not restored");
        auto stale = cmd(*run, "old_cancel", Cancel{first});
        stale.incarnation = previous;
        post(*run, std::move(stale), PostStatus::stale_incarnation);
        post(*run, cmd(*run, "submit", Submit{"fixture"}), PostStatus::duplicate);
        const auto replayed = until<CommandReplayed>(*run);
        check(replayed.request == first && std::get<CommandReplayed>(replayed.payload).state == "completed",
              "persistent duplicate did not identify original turn");
        post(*run, cmd(*run, "submit", Submit{"different"}), PostStatus::command_conflict);
        check(counts->generations == calls, "duplicate generated another response");
        submit(*run, "second", "remember first", {"second"});
        until<Done>(*run);
        const auto second = snapshot(*run, "second_view");
        check(second.history_items == 10 && counts->tools == 2, "history did not accumulate");
        finish(*run);
    }
    SessionState restored({"offline"}, Request({}, epoch(), tools()->advertisement(), {}));
    Journal journal(temp.session, [&](const Json& event) { restored.apply(event); });
    check(journal.diagnostic().empty() && restored.history.size() == 10 &&
          restored.history[0].text("content") == "fixture" && restored.history[5].text("content") == "remember first" &&
          restored.history[1].text("encrypted_content") == "synthetic_opaque_value", "native history changed");
    const auto page = restored.transcript_page({0, 16});
    check(page.total == 4 && page.entries[0].text == "fixture" && page.entries[0].role == "user" &&
          page.entries[1].role == "assistant" && page.entries[1].state == "completed" &&
          page.entries[2].text == "remember first" && page.entries[0].mode == AgentMode::basic,
          "transcript replay mismatch");
    const auto older = restored.transcript_page({2, 1});
    check(older.begin == 1 && older.end == 2 && older.entries.size() == 1, "transcript cursor mismatch");
    auto full = tools();
    auto plan_ad = full->read_only()->advertisement();
    Request config({}, epoch(), full->advertisement(), {});
    SessionState plan({"plan_session"}, config, plan_ad);
    auto init = plan.initialization();
    plan.apply(init);
    const auto request = plan.request_for("inspect", AgentMode::plan);
    check(request.epoch().id != plan.epoch.id && request.tools().revision == plan_ad.revision,
          "plan request provenance mismatch");
    auto accepted = Json::object({{"kind", Json::string("accepted")}, {"command", Json::string("p")},
        {"turn", Json::string("t")}, {"request", Json::string("r")}, {"request_number", Json::parse("1")},
        {"text", Json::string("inspect")}, {"mode", Json::string("plan")},
        {"operation_run", Json::string("run")}, {"next", save_request(request)}, {"compact", Json::parse("false")}});
    plan.apply(accepted);
    auto cancel = Json::object({{"kind", Json::string("cancel")}, {"turn", Json::string("t")}});
    plan.apply(cancel);
    check(plan.transcript_page({}).entries[0].state == "cancelled" &&
          plan.turn->receipt.mode == AgentMode::plan, "plan cancellation lost mode");
    SessionState replay({"plan_session"}, config, plan_ad);
    replay.apply(init); replay.apply(accepted); replay.apply(cancel);
    check(replay.transcript_page({}).entries[0].mode == AgentMode::plan &&
          replay.transcript_page({}).entries[0].state == "cancelled", "plan replay mismatch");
    check(save_request(plan.request_for("next")) == save_request(replay.request_for("next")), "base epoch changed");
    SessionState long_replay({"long_session"}, config);
    long_replay.apply(long_replay.initialization());
    std::shared_ptr<const TranscriptIndex> early;
    for (std::size_t i = 1; i <= 300; ++i) {
        const auto id = std::to_string(i);
        auto event = Json::object({{"kind", Json::string("accepted")}, {"command", Json::string("c" + id)},
            {"turn", Json::string("t" + id)}, {"request", Json::string("r" + id)},
            {"request_number", Json::parse(id)}, {"text", Json::string("item " + id)},
            {"operation_run", Json::string("run" + id)},
            {"next", save_request(long_replay.request_for("item " + id))}, {"compact", Json::parse("false")}});
        long_replay.apply(event);
        if (i == 64) early = long_replay.transcript();
        long_replay.apply(Json::object({{"kind", Json::string("cancel")}, {"turn", Json::string("t" + id)}}));
    }
    check(early->size() == 64 && early->page({}).entries.back().state == "generating",
          "transcript snapshot changed after append");
    const auto boundary = long_replay.transcript_page({130, 64});
    check(boundary.begin == 66 && boundary.end == 130 && boundary.total == 300 &&
          boundary.entries.front().text == "item 67" && boundary.entries.back().text == "item 130" &&
          boundary.entries.back().state == "cancelled", "long replay block cursor mismatch");
    const auto across = long_replay.transcript_page({61, 8, true});
    check(across.begin == 61 && across.end == 69 && across.entries.front().text == "item 62" &&
          across.entries.back().text == "item 69" && across.query == GetTranscript{61, 8, true},
          "forward block boundary or query mismatch");
    const auto oldest = long_replay.transcript_page({2, 16});
    check(oldest.begin == 0 && oldest.entries.front().text == "item 1", "long replay oldest entries lost");
    SessionState sized({"sized_session"}, config);
    sized.apply(sized.initialization());
    for (std::size_t i = 0; i < 24; ++i) {
        const auto id = std::to_string(i + 1);
        const std::string text(i < 4 ? 1 : max_submit_bytes, static_cast<char>('a' + i));
        sized.apply(Json::object({{"kind", Json::string("accepted")}, {"command", Json::string("c" + id)},
            {"turn", Json::string("t" + id)}, {"request", Json::string("r" + id)},
            {"request_number", Json::parse(id)}, {"text", Json::string(text)},
            {"operation_run", Json::string("run" + id)}, {"next", save_request(sized.request_for(text))},
            {"compact", Json::parse("false")}}));
        sized.apply(Json::object({{"kind", Json::string("cancel")}, {"turn", Json::string("t" + id)}}));
    }
    const auto forward = sized.transcript_page({4, 64, true});
    check(forward.begin == 4 && forward.end == 20 && forward.entries.size() == 16 &&
          forward.entries.front().text.front() == 'e' && forward.entries.back().text.front() == 't',
          "forward byte limit or cursor mismatch");
    const auto next_page = sized.transcript_page({forward.end, 64, true});
    check(next_page.begin == 20 && next_page.end == 24 && next_page.entries.front().text.front() == 'u',
          "forward continuation skipped entries");
    const auto reverse_page = sized.transcript_page({20, 64});
    check(reverse_page.begin == 4 && reverse_page.end == 20 && reverse_page.entries.size() == 16 && reverse_page.query == GetTranscript{20, 64},
          "backward byte limit mismatch");
    const auto start_page = sized.transcript_page({0, 4, true});
    check(start_page.begin == 0 && start_page.end == 4 && start_page.entries.front().text == "a",
          "forward zero cursor did not select start");
}

RuntimeIncarnation crash_at(const std::filesystem::path& path, const std::string& kind, unsigned occurrence = 1,
                            std::size_t compact_bytes = 256 * 1024, const std::string& id = "submit",
                            Generate generator = generate) {
    int signal_pipe[2];
    check(::pipe2(signal_pipe, O_CLOEXEC) == 0, "pipe");
    const auto child = ::fork();
    check(child >= 0, "fork");
    if (child == 0) {
        ::close(signal_pipe[0]);
        RuntimeIncarnation source;
        unsigned seen = 0;
        JournalOptions hooks{[&](JournalPoint point, std::string_view event) {
            if (point == JournalPoint::after_sync && event == kind && ++seen == occurrence) {
                const auto sent = ::write(signal_pipe[1], source.value.data(), source.value.size());
                if (sent != static_cast<ssize_t>(source.value.size())) ::_exit(3);
                for (;;) ::pause();
            }
        }};
        try {
            auto run = runtime(path, hooks, {.compact_bytes = compact_bytes}, compact, generator);
            source = run->incarnation();
            post(*run, cmd(*run, id, Submit{"fixture"}, {id == "submit" ? "turn" : id}));
            run->close_input();
            while (run->next_event()) {}
        } catch (...) {}
        ::_exit(2);
    }
    ::close(signal_pipe[1]);
    pollfd waiting{signal_pipe[0], POLLIN, 0};
    const auto ready = ::poll(&waiting, 1, 10000);
    std::string incarnation(32, '\0');
    const auto n = ready > 0 ? ::read(signal_pipe[0], incarnation.data(), incarnation.size()) : -1;
    ::close(signal_pipe[0]);
    ::kill(child, SIGKILL);
    int status = 0;
    check(::waitpid(child, &status, 0) == child, "waitpid");
    check(n == 32 && WIFSIGNALED(status), "child did not reach crash boundary");
    return {incarnation};
}

void crash_recovery() {
    struct Boundary { const char* kind; unsigned occurrence; unsigned tools_before; bool unknown, done; };
    const std::vector<Boundary> boundaries{
        {"accepted", 1, 0, false, false}, {"response", 1, 0, false, false},
        {"intent", 1, 0, true, false}, {"outcome", 1, 1, false, false},
        {"continued", 1, 1, false, false}, {"response", 2, 1, false, false},
        {"done", 1, 1, false, true}};
    for (const auto& boundary : boundaries) {
        counts->tools = counts->generations = counts->compactions = 0;
        Temp temp;
        const auto old = crash_at(temp.session, boundary.kind, boundary.occurrence);
        check(counts->tools == boundary.tools_before, "executor crossed durability barrier");
        auto run = runtime(temp.session);
        check(run->incarnation() != old, "incarnation reused after crash");
        const auto view = snapshot(*run);
        check(!view.storage_failed && view.durable && view.state == (boundary.done ? "completed" : "paused"),
              "incorrect recovered phase");
        const auto before = counts->generations.load();
        check(snapshot(*run, "still_paused").state == view.state && counts->generations == before,
              "recovery automatically dispatched");
        if (boundary.unknown) {
            check(view.recovery_operation && !view.resume_available, "intent without outcome was retried");
            post(*run, cmd(*run, "resume", Resume{*view.request}));
            const auto error = std::get<RuntimeError>(until<RuntimeError>(*run).payload);
            check(error.diagnostic == "recovery_required" && counts->tools == 0, "unknown effect Resume");
        } else if (!boundary.done) {
            check(view.resume_available, "safe recovery cannot Resume");
            post(*run, cmd(*run, "resume", Resume{*view.request}));
            until<Done>(*run);
            check(counts->tools == 1, "Resume repeated a known tool");
        }
        finish(*run);
    }
}

void storage_failure() {
    for (const std::string target : {"accepted", "outcome", "done"}) {
        counts->tools = counts->generations = 0;
        Temp temp;
        JournalOptions hooks{[&](JournalPoint point, std::string_view kind) {
            if (kind == target && point == JournalPoint::before_sync)
                throw Error(ErrorCode::contract, Stage::frontend, "injected_sync_failure");
        }};
        auto run = runtime(temp.session, hooks);
        post(*run, cmd(*run, "submit", Submit{"fixture"}));
        const auto error = std::get<RuntimeError>(until<RuntimeError>(*run).payload);
        check(error.diagnostic == "injected_sync_failure", "storage error hidden");
        auto view = snapshot(*run);
        check(view.storage_failed && !view.durable && view.state == "paused" &&
              counts->tools == (target == "accepted" ? 0u : 1u), "dispatch survived failed sync");
        if (target == "outcome")
            check(view.tools.size() == 1 && std::holds_alternative<ToolFinished>(view.tools.front()) &&
                  *std::get<ToolFinished>(view.tools.front()).output == R"({"ok":true,"value":"OMH_FIXED_VALUE_42"})",
                  "known outcome lost on fsync failure");
        post(*run, cmd(*run, "cancel", Cancel{*view.request}));
        until<Cancelled>(*run);
        run->close_input();
        while (run->next_event()) {}
    }
}

void cancel_on_blocked_disk() {
    Temp temp;
    Gate gate;
    JournalOptions hooks{[&](JournalPoint point, std::string_view kind) {
        if (kind == "accepted" && point == JournalPoint::before_sync) gate.hold();
    }};
    auto run = runtime(temp.session, hooks);
    post(*run, cmd(*run, "submit", Submit{"fixture"}));
    gate.wait();
    const auto view = snapshot(*run);
    check(view.request && !view.durable && view.worker_busy && counts->generations == 0, "accepted before fsync");
    post(*run, cmd(*run, "cancel", Cancel{*view.request}));
    const auto cancelled = snapshot(*run, "cancelled_view");
    check(cancelled.state == "cancelled" && counts->generations == 0, "disk blocked Cancel owner");
    gate.release();
    until<Cancelled>(*run);
    finish(*run);
    check(counts->tools == 0, "Cancel before durable acceptance invoked tool");
    run.reset();
    auto reopened = runtime(temp.session);
    check(snapshot(*reopened).state == "cancelled" && counts->generations == 0, "cancel was not durable");
    finish(*reopened);
}
void late_outcome_after_storage_failure() {
    Temp temp;
    Gate gate;
    executor_gate = &gate;
    JournalOptions hooks{[](JournalPoint point, std::string_view kind) {
        if (kind == "cancel" && point == JournalPoint::before_write)
            throw Error(ErrorCode::contract, Stage::frontend, "injected_write_failure");
    }};
    auto run = runtime(temp.session, hooks);
    const auto target = submit(*run);
    gate.wait();
    post(*run, cmd(*run, "cancel", Cancel{target}));
    until<RuntimeError>(*run);
    gate.release();
    const auto finished = std::get<ToolFinished>(until<ToolFinished>(*run).payload);
    check(finished.effects == Effects::known_result && finished.output &&
          *finished.output == R"({"ok":true,"value":"OMH_FIXED_VALUE_42"})", "late known outcome lost");
    const auto view = snapshot(*run);
    check(view.storage_failed && !view.durable && view.state == "cancelled" && counts->tools == 1,
          "late outcome continued execution or cancellation was lost");
    run->close_input();
    while (run->next_event()) {}
}

void compaction_recovery() {
    Temp temp;
    {
        auto run = runtime(temp.session, {}, {.compact_bytes = 300});
        submit(*run);
        until<Done>(*run);
        finish(*run);
    }
    crash_at(temp.session, "compacted", 1, 300, "second");
    check(counts->compactions == 1 && counts->tools == 1, "compact crossed wrong boundary");
    auto run = runtime(temp.session, {}, {.compact_bytes = 300});
    const auto view = snapshot(*run);
    check(view.compactions == 1 && view.history_items == 2 && view.state == "paused", "compacted epoch not recovered");
    post(*run, cmd(*run, "resume", Resume{*view.request}, {"second"}));
    until<Done>(*run);
    check(counts->compactions == 1 && counts->tools == 2, "Resume repeated compact");
    finish(*run);
    run.reset();
    SessionState state({"offline"}, Request({}, epoch(), tools()->advertisement(), {}));
    Journal journal(temp.session, [&](const Json& event) { state.apply(event); });
    check(journal.diagnostic().empty() && state.epoch.id.starts_with("compact_1_") && state.history.size() == 7 &&
          state.history[1].text("type") == "compaction", "compacted window did not survive Done");
    const auto page = state.transcript_page({});
    check(page.total == 4 && page.entries[0].text == "fixture" && page.entries[2].text == "fixture",
          "compaction erased transcript");
}

void compaction_failure() {
    Temp temp;
    unsigned attempts = 0;
    auto compactor = [&](const Request& request, std::stop_token stop) -> Result<CompactedContext> {
        if (++attempts == 1) return Error(ErrorCode::contract, Stage::admission, "injected_compact_failure");
        return compact(request, stop);
    };
    auto run = runtime(temp.session, {}, {.compact_bytes = 300}, compactor);
    submit(*run);
    until<Done>(*run);
    submit(*run, "second", "next", {"second"});
    const auto error = std::get<RuntimeError>(until<RuntimeError>(*run).payload);
    check(error.diagnostic == "injected_compact_failure", "compact failure hidden");
    const auto view = snapshot(*run);
    check(view.state == "paused" && view.compactions == 0 && view.history_items == 5 &&
          view.resume_available && !view.storage_failed, "failed compact changed prior history");
    post(*run, cmd(*run, "retry", Resume{*view.request}, {"second"}));
    until<Done>(*run);
    check(attempts == 2 && counts->tools == 2, "Resume did not retry compact");
    finish(*run);
}

void repeated_resume() {
    Temp temp;
    Generate failing = [](const Request&, std::stop_token) -> Result<CompletedResponse> {
        return Error(ErrorCode::contract, Stage::admission, "fixture_provider_failure");
    };
    Runtime run({"offline"}, epoch(), tools(), failing, {}, {temp.session, compact, {}});
    const auto target = submit(run);
    until<RuntimeError>(run);
    post(run, cmd(run, "resume_once", Resume{target}));
    until<RuntimeError>(run);
    post(run, cmd(run, "resume_once", Resume{target}));
    until<CommandReplayed>(run);
    const auto view = snapshot(run);
    check(view.state == "paused" && !view.storage_failed && view.durable, "duplicate Resume damaged journal");
    finish(run);
}

void compact_item_budget() {
    Temp temp;
    Generate small = [](const Request& request, std::stop_token) -> Result<CompletedResponse> {
        const auto generation = ++counts->generations;
        std::vector<Json> items;
        for (unsigned index = 0; index < 64; ++index)
            items.push_back(Json::object({{"type", Json::string("message")},
                {"id", Json::string("m_" + std::to_string(generation) + "_" + std::to_string(index))},
                {"role", Json::string("assistant")}, {"status", Json::string("completed")}, {"content", Json::array({})}}));
        return load_response(Json::object({{"request", save_request(request)},
            {"id", Json::string("r_" + std::to_string(generation))},
            {"model", Json::string(request.binding().returned_model)}, {"items", Json::array(items)}}));
    };
    Runtime run({"offline"}, epoch(), tools(), small, {.compact_bytes = 4 * 1024 * 1024},
                {temp.session, compact, {}});
    for (unsigned turn = 0; turn < 65; ++turn) {
        const auto id = "turn_" + std::to_string(turn);
        submit(run, id, "x", {id});
        until<Done>(run);
    }
    const auto view = snapshot(run);
    check(view.compactions > 0 && !view.storage_failed && view.history_items < compact_item_threshold,
          "item limit prevented automatic compact");
    finish(run);
}

void cancel_before_tool_dispatch() {
    Temp temp;
    Gate gate;
    JournalOptions hooks{[&](JournalPoint point, std::string_view kind) {
        if (kind == "intent" && point == JournalPoint::before_sync) gate.hold();
    }};
    auto run = runtime(temp.session, hooks);
    const auto target = submit(*run);
    gate.wait();
    post(*run, cmd(*run, "cancel", Cancel{target}));
    check(snapshot(*run).state == "cancelled", "intent sync blocked Cancel");
    gate.release();
    until<Cancelled>(*run);
    run->close_input();
    while (auto event = run->next_event())
        check(!std::holds_alternative<ToolStarted>(event->payload) && !std::holds_alternative<ToolFinished>(event->payload),
              "undispatched intent emitted tool activity");
    check(counts->tools == 0, "tool executed after intent cancellation");
    run.reset();
    auto reopened = runtime(temp.session);
    const auto view = snapshot(*reopened);
    check(view.state == "cancelled" && view.tools.empty() && !view.recovery_operation,
          "resolved not_started intent was not recovered");
    finish(*reopened);
}

void cancel_compaction() {
    Temp temp;
    Gate gate;
    Compact blocked = [&](const Request& request, std::stop_token) -> Result<CompactedContext> {
        auto result = compact(request, {});
        gate.hold();
        return result;
    };
    auto run = runtime(temp.session, {}, {.compact_bytes = 300}, blocked);
    submit(*run);
    until<Done>(*run);
    const auto target = submit(*run, "second", "next", {"second"});
    gate.wait();
    post(*run, cmd(*run, "cancel", Cancel{target}, {"second"}));
    until<Cancelled>(*run);
    check(snapshot(*run).compactions == 0, "cancelled compaction changed epoch");
    gate.release();
    finish(*run);
    run.reset();
    auto reopened = runtime(temp.session, {}, {.compact_bytes = 300});
    const auto view = snapshot(*reopened);
    check(view.state == "cancelled" && view.compactions == 0 && view.history_items == 5 && counts->tools == 1,
          "late compaction result replaced history");
    finish(*reopened);
}

void oversized_compaction() {
    Temp temp;
    Compact oversized = [](const Request& request, std::stop_token) {
        std::vector<Json> output(4000, Json::parse(R"({"role":"user","content":"x"})"));
        output.push_back(Json::parse(R"({"type":"compaction","id":"test","encrypted_content":"opaque"})"));
        return CompactedContext::admit(request, Json::object({{"object", Json::string("response.compaction")},
            {"id", Json::string("test")}, {"output", Json::array(output)}}));
    };
    auto run = runtime(temp.session, {}, {.compact_bytes = 300}, oversized);
    submit(*run);
    until<Done>(*run);
    submit(*run, "second", "next", {"second"});
    check(std::get<RuntimeError>(until<RuntimeError>(*run).payload).diagnostic == "compaction_item_limit",
          "oversized compaction was not rejected");
    const auto view = snapshot(*run);
    check(!view.storage_failed && view.state == "paused" && view.history_items == 5 && view.compactions == 0,
          "invalid compaction damaged journal");
    finish(*run);
}

void journal_parts_limit() {
    Temp temp;
    std::filesystem::create_directory(temp.session);
    std::string previous(64, '0');
    for (unsigned index = 0; index < 4096; ++index) {
        const auto value = std::to_string(index);
        const auto padded = std::string(20 - value.size(), '0') + value;
        const auto header = index == 4095 ? std::string("OMHJ1 ") : "OMHJ1 " + padded + " " + previous + "\n";
        { std::ofstream file(temp.session / ("part-" + padded + ".log")); file << header; }
        previous = sha256(header);
    }
    Journal journal(temp.session, [&](const Json&) {});
    check(journal.diagnostic().empty(), "limit fixture invalid");
    rejects([&] { journal.append(record(1)); }, "journal_parts_limit");
    check(parts(temp.session).size() == 4096, "unreadable part created");
}

void batch_recovery() {
    Temp temp;
    Generate two_calls = [](const Request& request, std::stop_token) -> Result<CompletedResponse> {
        ++counts->generations;
        std::vector<Json> items;
        if (request.input().back().has("type") && request.input().back().text("type") == "function_call_output") {
            check(request.input().size() == 5 && request.input()[3].text("call_id") == "call_1" &&
                  request.input()[4].text("call_id") == "call_2", "batch continuation changed");
            items.push_back(Json::parse(R"({"type":"message","id":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"done"}]})"));
        } else {
            for (unsigned index = 1; index <= 2; ++index)
                items.push_back(Json::object({{"type", Json::string("function_call")},
                    {"id", Json::string("item_" + std::to_string(index))},
                    {"call_id", Json::string("call_" + std::to_string(index))},
                    {"namespace", Json::string("omh")}, {"name", Json::string("get_test_value")},
                    {"status", Json::string("completed")}, {"arguments", Json::string("{}")}}));
        }
        return load_response(Json::object({{"request", save_request(request)}, {"id", Json::string("response")},
            {"model", Json::string(request.binding().returned_model)}, {"items", Json::array(items)}}));
    };
    crash_at(temp.session, "outcome", 1, 256 * 1024, "submit", two_calls);
    check(counts->tools == 1, "next tool dispatched before prior outcome durability");
    auto run = runtime(temp.session, {}, {}, compact, two_calls);
    const auto view = snapshot(*run);
    check(view.tools.size() == 1 && view.resume_available, "completed batch prefix not recovered");
    post(*run, cmd(*run, "resume", Resume{*view.request}));
    until<Done>(*run);
    check(counts->tools == 2 && snapshot(*run, "finished_batch").tools.size() == 2,
          "Resume duplicated prefix or lost next tool");
    finish(*run);
}
void dispatch_guard() {
    Temp temp;
    std::atomic<bool> permitted = true, revoke_once = true;
    JournalOptions hooks{[&](JournalPoint point, std::string_view kind) {
        if (point == JournalPoint::after_sync && kind == "intent" && revoke_once.exchange(false))
            permitted = false;
    }};
    Runtime run({"offline"}, epoch(), tools(), generate, {}, {temp.session, compact, hooks, {}, 0,
        [&](std::uint64_t, std::stop_token) {
            require(permitted, "auth_required", Stage::frontend, ErrorCode::validation);
        }});
    const auto target = submit(run);
    const auto failure = std::get<RuntimeError>(until<RuntimeError>(run).payload);
    const auto view = snapshot(run);
    check(failure.diagnostic == "auth_required" && counts->tools == 0 && view.tools.empty() &&
          view.resume_available && !view.recovery_operation, "guard after durable intent did not stop tool");
    permitted = true;
    post(run, cmd(run, "resume", Resume{target}));
    until<Done>(run);
    check(counts->tools == 1, "safe not_started guard result could not Resume");
    finish(run);
}
} // namespace

int main(int argc, char** argv) {
    try {
        auto memory = ::mmap(nullptr, sizeof(Counters), PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
        check(memory != MAP_FAILED, "mmap");
        counts = new(memory) Counters;
        check(argc == 2, "suite required");
        const std::string suite = argv[1];
        if (suite == "journal") journal_integrity();
        else if (suite == "conflict") journal_conflict();
        else if (suite == "history") history_and_dedup();
        else if (suite == "crash") crash_recovery();
        else if (suite == "storage-failure") storage_failure();
        else if (suite == "blocked-disk") cancel_on_blocked_disk();
        else if (suite == "late-outcome") late_outcome_after_storage_failure();
        else if (suite == "compaction") compaction_recovery();
        else if (suite == "compact-failure") compaction_failure();
        else if (suite == "resume-repeat") repeated_resume();
        else if (suite == "item-budget") compact_item_budget();
        else if (suite == "intent-cancel") cancel_before_tool_dispatch();
        else if (suite == "compact-cancel") cancel_compaction();
        else if (suite == "compact-budget") oversized_compaction();
        else if (suite == "parts-limit") journal_parts_limit();
        else if (suite == "batch-recovery") batch_recovery();
        else if (suite == "dispatch-guard") dispatch_guard();
        else throw std::runtime_error("unknown suite");
        counts->~Counters();
        ::munmap(memory, sizeof(Counters));
        return 0;
    } catch (const std::exception& error) { std::cerr << error.what() << '\n'; return 1; }
}
