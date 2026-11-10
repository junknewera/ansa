#include "omh/offline.hpp"
#include "omh/transport.hpp"
#include "../src/runtime_inbox.hpp"
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <fstream>
#include <iostream>
#include <mutex>
#include <stdexcept>

namespace {
using namespace omh;
using namespace std::chrono_literals;

void check(bool ok, const char* diagnostic) {
    if (!ok) throw std::runtime_error(diagnostic);
}

template<class T> T unwrap(Result<T> result) {
    if (const auto* error = std::get_if<Error>(&result)) throw *error;
    return std::get<T>(std::move(result));
}

class Gate {
    std::mutex mutex_;
    std::condition_variable changed_;
    bool entered_ = false, released_ = false;
    std::stop_token stop_;
public:
    void hold(std::stop_token stop) {
        std::unique_lock lock(mutex_);
        stop_ = stop;
        entered_ = true;
        changed_.notify_all();
        check(changed_.wait_for(lock, 5s, [&] { return released_; }), "fixture gate release timeout");
    }
    void wait() {
        std::unique_lock lock(mutex_);
        check(changed_.wait_for(lock, 5s, [&] { return entered_; }), "fixture gate entry timeout");
    }
    bool stopped() {
        std::lock_guard lock(mutex_);
        return stop_.stop_requested();
    }
    void release() {
        std::lock_guard lock(mutex_);
        released_ = true;
        changed_.notify_all();
    }
};

std::atomic<unsigned> invocations = 0;
Gate* executor_gate = nullptr; // Set before runtime construction and read until its worker joins.

ToolOutcome counted(const Json& args, std::stop_token stop) {
    ++invocations;
    if (executor_gate) executor_gate->hold(stop);
    return test_value_tool().execute(args, stop);
}

std::shared_ptr<const ToolSnapshot> tools() {
    ToolRegistry registry;
    auto tool = test_value_tool();
    tool.execute = counted;
    registry.register_tool(std::move(tool));
    return registry.snapshot();
}

ContextEpoch epoch() {
    return {"fixture_epoch", "Use the pure fixture tool", Json::parse(R"({"mode":"offline","revision":1})")};
}

RuntimeCommand command(const Runtime& runtime, std::string id, CommandPayload payload, TurnId turn = {"turn"}) {
    return {1, {"offline"}, std::move(turn), runtime.incarnation(), {std::move(id)}, std::move(payload)};
}

void post(Runtime& runtime, RuntimeCommand cmd, PostStatus expected = PostStatus::accepted) {
    check(unwrap(runtime.post(std::move(cmd))) == expected, "unexpected post status");
}

std::optional<RuntimeEvent> next(Runtime& runtime) {
    auto event = runtime.next_event();
    if (event) {
        validate(*event);
        check(event->incarnation == runtime.incarnation(), "event incarnation");
    }
    return event;
}

template<class Payload, class Predicate>
RuntimeEvent until(Runtime& runtime, Predicate match) {
    while (auto event = next(runtime)) {
        const auto* payload = std::get_if<Payload>(&event->payload);
        if (payload && match(*payload)) return std::move(*event);
    }
    throw std::runtime_error("missing event");
}

template<class Payload>
RuntimeEvent until(Runtime& runtime) {
    return until<Payload>(runtime, [](const auto&) { return true; });
}

RequestId submit(Runtime& runtime) {
    post(runtime, command(runtime, "submit", Submit{"fixture"}));
    const auto accepted = until<CommandAccepted>(runtime);
    const auto& payload = std::get<CommandAccepted>(accepted.payload);
    check(payload.command.value == "submit" && payload.request && accepted.request == payload.request,
          "accepted command identity");
    return *payload.request;
}

RuntimeEvent get_snapshot(Runtime& runtime, std::uint64_t after_sequence, const std::string& id) {
    post(runtime, command(runtime, id, GetSnapshot{after_sequence}));
    auto event = until<FrontendSnapshot>(runtime, [&](const FrontendSnapshot& s) { return s.command == CommandId{id}; });
    check(std::get<FrontendSnapshot>(event.payload).through_sequence == event.stream_sequence,
          "snapshot cursor does not cover its own event");
    return event;
}

void blocked_generation(bool cancel_at_completion) {
    Gate gate, stop_callback_gate;
    std::atomic<bool> returned_success = false, stop_callback_finished = false;
    Runtime runtime({"offline"}, epoch(), tools(), [&](const Request& request, std::stop_token stop) {
        const std::stop_callback callback(stop, [&] {
            if (!cancel_at_completion) {
                try { stop_callback_gate.hold(stop); } catch (...) {}
                stop_callback_finished = true;
            }
        });
        if (!cancel_at_completion) gate.hold(stop);
        auto result = offline_generate(request); // Return a successful late result even after stop.
        if (cancel_at_completion) gate.hold(stop);
        returned_success = std::holds_alternative<CompletedResponse>(result);
        return result;
    });
    const auto target = submit(runtime);
    gate.wait();
    post(runtime, command(runtime, "cancel", Cancel{target}));
    if (cancel_at_completion) gate.release();
    const auto cancelled = until<Cancelled>(runtime);
    check(cancelled.request == target && std::get<Cancelled>(cancelled.payload).command.value == "cancel",
          "cancel identity");

    if (!cancel_at_completion) {
        stop_callback_gate.wait();
        const auto view = get_snapshot(runtime, cancelled.stream_sequence, "active_snapshot");
        const auto& snapshot = std::get<FrontendSnapshot>(view.payload);
        check(snapshot.state == "cancelled" && snapshot.worker_busy && snapshot.request == target && gate.stopped(),
              "owner did not cancel while generation was blocked");
        check(!stop_callback_finished, "stop callback blocked the control owner");
        stop_callback_gate.release();
        gate.release();
    }

    runtime.close_input();
    std::optional<FrontendSnapshot> last;
    while (auto event = next(runtime)) {
        check(!std::holds_alternative<ToolStarted>(event->payload) && !std::holds_alternative<Done>(event->payload),
              "late completion continued cancelled turn");
        if (const auto* snapshot = std::get_if<FrontendSnapshot>(&event->payload)) last = *snapshot;
    }
    check(returned_success && last && last->state == "cancelled" && !last->worker_busy && invocations == 0,
          "late success was not discarded");
}

void pending_cancel_priority() {
    using namespace omh::detail;
    RuntimeIncarnation incarnation{"incarnation"};
    WorkIdentity identity{incarnation, {"turn"}, {"request_1"}};
    const auto registry = tools();
    const auto request = Request({}, epoch(), registry->advertisement(), {});
    auto response = unwrap(offline_generate(request));
    OperationIds operations("test");
    auto batch = unwrap(PreparedBatch::prepare(response, registry, operations));
    OwnerInbox inbox;
    inbox.completion.emplace(Completion{identity, RunTools{std::move(response), std::move(batch)}});
    inbox.cancel = RuntimeCommand{1, {"offline"}, identity.turn, incarnation, {"cancel"}, Cancel{identity.request}};
    check(inbox.next(true) == Action::cancel, "a pending success bypassed pending Cancel");
    inbox.cancel.reset();
    check(inbox.next(true) == Action::complete, "pending completion was not ready");
    inbox.aborting = true;
    check(inbox.next(true) == Action::stop, "shutdown dispatched pending completion");
}

void admission() {
    RuntimeIncarnation previous;
    RequestId previous_request;
    {
        Runtime old({"offline"}, epoch(), tools(), offline_generate);
        previous = old.incarnation();
        previous_request = submit(old);
        old.close_input();
        until<Done>(old);
    }
    invocations = 0;
    Gate gate;
    Runtime runtime({"offline"}, epoch(), tools(), [&](const Request& request, std::stop_token stop) {
        if (request.input().front().text("content") == "fixture") gate.hold(stop);
        return offline_generate(request, stop);
    }, {.commands = 1, .submit_ids = 3});
    const auto target = submit(runtime);
    gate.wait();
    check(target == previous_request && previous != runtime.incarnation(), "fixture must reuse request ID across runtimes");
    auto stale = command(runtime, "stale", Cancel{previous_request});
    stale.incarnation = previous;
    post(runtime, stale, PostStatus::stale_incarnation);
    post(runtime, command(runtime, "wrong_turn", Cancel{target}, {"another"}), PostStatus::stale_target);

    post(runtime, command(runtime, "submit", Submit{"changed"}, {"another"}), PostStatus::duplicate);
    post(runtime, command(runtime, "queued", Submit{"next"}, {"next"}));
    post(runtime, command(runtime, "queued", Submit{"retry"}, {"different"}), PostStatus::duplicate);
    post(runtime, command(runtime, "overflow", Submit{"last"}, {"last"}), PostStatus::queue_full);
    const auto active = get_snapshot(runtime, 0, "active");
    check(active.request == target && std::get<FrontendSnapshot>(active.payload).state == "generating" &&
          !gate.stopped(), "stale Cancel affected active request");

    post(runtime, command(runtime, "cancel", Cancel{target}));
    until<Cancelled>(runtime);
    gate.release();
    const auto queued = until<CommandAccepted>(runtime);
    check(queued.turn.value == "next" && std::get<CommandAccepted>(queued.payload).command.value == "queued",
          "duplicate Submit started a turn");

    post(runtime, command(runtime, "overflow", Submit{"last"}, {"last"}));
    post(runtime, command(runtime, "new_id", Submit{"extra"}), PostStatus::command_id_limit);
    post(runtime, command(runtime, "submit", Submit{"duplicate"}), PostStatus::duplicate);
    runtime.close_input();
    std::vector<std::string> done;
    while (auto event = next(runtime)) {
        if (std::holds_alternative<Done>(event->payload)) done.push_back(event->turn.value);
    }
    check(done == std::vector<std::string>{"next", "last"} && invocations == 2,
          "queued retry or dedup produced the wrong turns");
}

void resync() {
    Runtime runtime({"offline"}, epoch(), tools(), offline_generate, {.events = 2});
    const auto initial = until<FrontendSnapshot>(runtime);
    const auto cursor = initial.stream_sequence;
    post(runtime, command(runtime, "submit", Submit{"fixture"}));
    // The final transition emits text, Done and a snapshot under the owner lock.
    // A two-event outbox must report a gap even if the reader keeps up.
    bool gap = false;
    while (true) {
        auto event = next(runtime);
        check(event.has_value(), "unexpected EOF");
        if (const auto* missing = std::get_if<ResyncRequired>(&event->payload)) {
            check(missing->oldest_available == event->stream_sequence + 1 && event->stream_sequence > cursor + 1,
                  "resync marker does not identify the cursor gap");
            gap = true;
        }
        if (const auto* snapshot = std::get_if<FrontendSnapshot>(&event->payload);
            snapshot && snapshot->state == "completed") break;
    }
    check(gap, "bounded outbox did not report loss");
    const auto recovered = get_snapshot(runtime, cursor, "recover");
    const auto& snapshot = std::get<FrontendSnapshot>(recovered.payload);
    check(recovered.turn.value == "turn" && recovered.request == snapshot.request && snapshot.request &&
          snapshot.submit == CommandId{"submit"} && snapshot.state == "completed" && !snapshot.worker_busy &&
          snapshot.completed_tools == 1 && snapshot.text == "OMH_TOOL_OK:OMH_FIXED_VALUE_42" &&
          snapshot.diagnostic.empty() && snapshot.tools.size() == 1 &&
          std::get<ToolFinished>(snapshot.tools.front()).call_id == "fixture_call" && invocations == 1,
          "resync lost public turn state");

    post(runtime, command(runtime, "future_cursor", GetSnapshot{recovered.stream_sequence + 100}));
    until<RuntimeError>(runtime, [](const RuntimeError& error) {
        return error.command == CommandId{"future_cursor"} && error.diagnostic == "resync_cursor_ahead";
    });
    runtime.close_input();
    check(!next(runtime), "events after draining closed input");
}

std::string fixture() {
    std::ifstream stream(std::string(OMH_FIXTURES) + "/round1.sse", std::ios::binary);
    check(bool(stream), "missing fixture");
    return {std::istreambuf_iterator<char>(stream), {}};
}

std::string two_calls(bool valid_args) {
    auto bytes = fixture();
    const auto terminal = bytes.rfind("data: ");
    check(terminal != std::string::npos, "missing terminal");
    const auto second = Json::object({{"type", Json::string("function_call")}, {"id", Json::string("fc_b")},
        {"call_id", Json::string("call_b")}, {"name", Json::string("get_test_value")},
        {"namespace", Json::string("omh")}, {"arguments", Json::string(valid_args ? "{}" : R"({"extra":1})")},
        {"status", Json::string("completed")}}).dump();
    auto ending = bytes.substr(terminal);
    ending.replace(ending.find("number\":8"), 9, "number\":10");
    ending.insert(ending.rfind(']'), "," + second);
    bytes.resize(terminal);
    bytes += R"(data: {"type":"response.output_item.added","sequence_number":8,"output_index":2,"item":)" + second + "}\n\n";
    bytes += R"(data: {"type":"response.output_item.done","sequence_number":9,"output_index":2,"item":)" + second + "}\n\n";
    return bytes + ending;
}

void reject_response() {
    const auto bytes = fixture();
    const auto terminal = bytes.rfind("data: ");
    check(terminal != std::string::npos, "missing terminal");
    const auto prefix = bytes.substr(0, terminal);
    const std::vector<std::pair<std::string, std::string>> cases{
        {prefix + "data: {\"type\":\"response.failed\",\"sequence_number\":8}\n\n", "response_failed"},
        {prefix + "data: {\"type\":\"response.incomplete\",\"sequence_number\":8}\n\n", "response_incomplete"},
        {prefix + "data: {malformed}\n\n", "json_invalid"},
        {prefix, "eof_before_completed"},
        {two_calls(false), "tool_arguments_rejected"},
    };
    for (const auto& [wire, diagnostic] : cases) {
        Runtime runtime({"offline"}, epoch(), tools(), [&](const Request& request, std::stop_token stop) {
            return decode_offline(request, {200, {"text/event-stream"}}, {wire}, {}, MimePolicy::strict, stop);
        });
        submit(runtime);
        runtime.close_input();
        bool failed = false;
        while (auto event = next(runtime)) {
            if (const auto* error = std::get_if<RuntimeError>(&event->payload)) {
                if (error->diagnostic != diagnostic)
                    throw std::runtime_error("expected " + diagnostic + ", got " + error->diagnostic);
                failed = true;
            }
            check(!std::holds_alternative<ToolStarted>(event->payload) && !std::holds_alternative<Done>(event->payload),
                  "invalid provider response reached dispatch");
        }
        check(failed && invocations == 0, "invalid response invoked an executor");
    }
}

void cancel_between_tools() {
    Gate gate;
    executor_gate = &gate;
    const auto bytes = two_calls(true);
    Runtime runtime({"offline"}, epoch(), tools(), [&](const Request& request, std::stop_token stop) {
        return decode_offline(request, {200, {"text/event-stream"}}, {bytes}, {}, MimePolicy::strict, stop);
    });
    const auto target = submit(runtime);
    const auto started = until<ToolStarted>(runtime);
    gate.wait();
    const auto active = get_snapshot(runtime, started.stream_sequence, "active_tool");
    const auto& activity = std::get<FrontendSnapshot>(active.payload).tools;
    check(activity.size() == 1 && std::get<ToolStarted>(activity.front()).operation ==
          std::get<ToolStarted>(started.payload).operation, "snapshot lost active tool");
    post(runtime, command(runtime, "cancel", Cancel{target}));
    until<Cancelled>(runtime);
    gate.release();
    runtime.close_input();
    const auto finished = std::get<ToolFinished>(until<ToolFinished>(runtime).payload);
    const auto& tool = std::get<ToolStarted>(started.payload);
    check(finished.operation == tool.operation && finished.call_id == tool.call_id &&
          finished.execution == Execution::succeeded && finished.effects == Effects::known_result &&
          finished.output && *finished.output == R"({"ok":true,"value":"OMH_FIXED_VALUE_42"})",
          "cancel lost result for a dispatched tool");
    const auto snapshot = std::get<FrontendSnapshot>(until<FrontendSnapshot>(runtime,
        [](const FrontendSnapshot& s) { return !s.worker_busy; }).payload);
    check(snapshot.state == "cancelled" && snapshot.completed_tools == 1 && invocations == 1,
          "cancel allowed another tool or lost the first outcome");
    check(!next(runtime), "cancelled batch continued after recording outcome");
}

void roundtrip() {
    Runtime runtime({"offline"}, epoch(), tools(), offline_generate);
    const auto target = submit(runtime);
    runtime.close_input();
    const auto started = until<ToolStarted>(runtime);
    const auto finished = until<ToolFinished>(runtime);
    const auto& start = std::get<ToolStarted>(started.payload);
    const auto& finish = std::get<ToolFinished>(finished.payload);
    check(started.request == target && finished.request == target && start.operation == finish.operation &&
          start.call_id == "fixture_call" && finish.call_id == start.call_id &&
          finish.execution == Execution::succeeded && finish.effects == Effects::known_result, "tool event identity");
    const auto text = until<TextDelta>(runtime);
    const auto done = until<Done>(runtime);
    const auto terminal = until<FrontendSnapshot>(runtime);
    check(text.request == target && done.request == target && std::get<TextDelta>(text.payload).text ==
          "OMH_TOOL_OK:OMH_FIXED_VALUE_42", "tool output did not reach continuation");
    const auto& snapshot = std::get<FrontendSnapshot>(terminal.payload);
    check(snapshot.state == "completed" && !snapshot.worker_busy && snapshot.completed_tools == 1 &&
          invocations == 1 && !next(runtime), "unexpected terminal state");
}

void shutdown() {
    std::mutex mutex;
    std::condition_variable_any changed;
    unsigned calls = 0;
    std::atomic<bool> stopped = false;
    {
        Runtime runtime({"offline"}, epoch(), tools(), [&](const Request& request, std::stop_token stop) {
            std::unique_lock lock(mutex);
            ++calls;
            changed.notify_all();
            changed.wait(lock, stop, [] { return false; });
            stopped = stop.stop_requested();
            return offline_generate(request);
        });
        submit(runtime);
        {
            std::unique_lock lock(mutex);
            check(changed.wait_for(lock, 5s, [&] { return calls > 0; }), "worker did not start");
        }
        post(runtime, command(runtime, "queued", Submit{"queued"}, {"queued"}));
    }
    check(stopped && calls == 1 && invocations == 0, "destruction dispatched queued or late work");
}
} // namespace

int main(int argc, char** argv) {
    try {
        check(argc == 2, "suite required");
        const std::string_view suite = argv[1];
        if (suite == "cancel") blocked_generation(false);
        else if (suite == "priority") { pending_cancel_priority(); blocked_generation(true); }
        else if (suite == "admission") admission();
        else if (suite == "resync") resync();
        else if (suite == "invalid-response") reject_response();
        else if (suite == "tool-cancel") cancel_between_tools();
        else if (suite == "roundtrip") roundtrip();
        else if (suite == "shutdown") shutdown();
        else throw std::runtime_error("unknown suite");
        return 0;
    } catch (const std::exception& error) {
        std::cerr << error.what() << '\n';
        return 1;
    }
}
