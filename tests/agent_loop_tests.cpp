#include "omh/runtime.hpp"
#include "omh/session.hpp"
#include "omh/process.hpp"
#include <atomic>
#include <condition_variable>
#include <fstream>
#include <iostream>
#include <sys/wait.h>
#include <unistd.h>

namespace {
using namespace omh;
void check(bool value, const char* text) { if (!value) throw std::runtime_error(text); }
template<class T> T unwrap(Result<T> value) {
    if (auto* error = std::get_if<Error>(&value)) throw *error;
    return std::get<T>(std::move(value));
}
struct Temp {
    std::filesystem::path root, path;
    Temp() {
        char pattern[] = "/tmp/ansa-loop-XXXXXX";
        const auto directory = ::mkdtemp(pattern); check(directory, "mkdtemp");
        root = directory; path = root / "session";
    }
    ~Temp() { std::error_code error; std::filesystem::remove_all(root, error); }
};
ContextEpoch epoch() { return {"multi_round_v1", "test", Json::parse("{}")}; }
std::size_t outputs(const Request& request) {
    return static_cast<std::size_t>(std::count_if(request.input().begin(), request.input().end(), [](const Json& item) {
        return item.has("type") && item.text("type") == "function_call_output";
    }));
}
CompletedResponse response(const Request& request, unsigned rounds) {
    const auto index = outputs(request);
    const auto suffix = std::to_string(index);
    Json item = index < rounds ? Json::object({{"type", Json::string("function_call")},
        {"id", Json::string("item_" + suffix)}, {"call_id", Json::string("call_" + suffix)},
        {"namespace", Json::string("omh")}, {"name", Json::string("get_test_value")},
        {"arguments", Json::string("{}")}, {"status", Json::string("completed")}}) :
        Json::object({{"type", Json::string("message")}, {"id", Json::string("message_" + suffix)},
            {"role", Json::string("assistant")}, {"status", Json::string("completed")},
            {"content", Json::array({Json::parse(R"({"type":"output_text","text":"MULTI_OK"})")})}});
    return load_response(Json::object({{"request", save_request(request)}, {"id", Json::string("response_" + suffix)},
        {"model", Json::string(request.binding().returned_model)}, {"items", Json::array({item})}}));
}
std::shared_ptr<const ToolSnapshot> tools(std::atomic<unsigned>& executions, bool known_failure = false) {
    ToolRegistry registry;
    auto definition = test_value_tool();
    definition.execute = [&, known_failure](const Json&, std::stop_token) {
        ++executions;
        return ToolOutcome{known_failure ? Execution::failed : Execution::succeeded,
                           Effects::known_result, R"({"exit_code":7})"};
    };
    registry.register_tool(std::move(definition)); return registry.snapshot();
}
RuntimeCommand command(Runtime& runtime, std::string id, CommandPayload payload) {
    return {1, {"multi"}, {"turn"}, runtime.incarnation(), {std::move(id)}, std::move(payload)};
}
void post(Runtime& runtime, std::string id, CommandPayload payload) {
    check(unwrap(runtime.post(command(runtime, std::move(id), std::move(payload)))) == PostStatus::accepted, "post");
}
template<class T> RuntimeEvent until(Runtime& runtime) {
    while (auto event = runtime.next_event()) {
        if (std::holds_alternative<T>(event->payload)) return *event;
        if constexpr (!std::is_same_v<T, RuntimeError>)
            if (const auto* error = std::get_if<RuntimeError>(&event->payload)) throw std::runtime_error(error->diagnostic);
    }
    throw std::runtime_error("missing event");
}
void finish(Runtime& runtime) { runtime.close_input(); while (runtime.next_event()) {} }
void roundtrip(bool failure) {
    Temp temp;
    std::atomic<unsigned> executed = 0;
    auto snapshot = tools(executed, failure);
    {
        Runtime runtime({"multi"}, epoch(), snapshot,
            [](const Request& request, std::stop_token) -> Result<CompletedResponse> { return response(request, 3); },
            {.tool_rounds=4, .tool_calls=8}, {temp.path});
        post(runtime, "submit", Submit{"three steps"}); until<Done>(runtime); finish(runtime);
    }
    check(executed == 3, "wrong executor count");
    SessionState state({"multi"}, Request({}, epoch(), snapshot->advertisement(), {}));
    Journal journal(temp.path, [&](const Json& record) { state.apply(record); });
    check(journal.diagnostic().empty() && state.turn->rounds == 3 && state.turn->previous_results.size() == 3,
          "multi-round journal did not recover");
    for (std::size_t index = 0; index < 3; ++index)
        check(state.turn->previous_results[index].operation_id.ends_with(":op:" + std::to_string(index + 1)),
              "operation IDs repeated across rounds");
    check(state.history.size() == 8, "native continuation lost earlier rounds");
}
void limit() {
    Temp temp; std::atomic<unsigned> executed = 0;
    Runtime runtime({"multi"}, epoch(), tools(executed),
        [](const Request& request, std::stop_token) -> Result<CompletedResponse> { return response(request, 3); },
        {.tool_rounds=1, .tool_calls=2}, {temp.path});
    post(runtime, "submit", Submit{"bounded"});
    check(std::get<RuntimeError>(until<RuntimeError>(runtime).payload).diagnostic == "tool_round_limit" && executed == 1,
          "round limit did not prevent execution");
    auto view = std::get<FrontendSnapshot>(until<FrontendSnapshot>(runtime).payload);
    post(runtime, "resume_1", Resume{*view.request});
    until<RuntimeError>(runtime);
    check(executed == 2, "Resume repeated a completed round");
    view = std::get<FrontendSnapshot>(until<FrontendSnapshot>(runtime).payload);
    post(runtime, "resume_2", Resume{*view.request});
    until<Done>(runtime); finish(runtime);
    check(executed == 3, "explicit Resume failed");
}
void crash() {
    for (const std::string boundary : {"response", "intent", "outcome", "continued"}) {
        Temp temp;
        const auto child = ::fork(); check(child >= 0, "fork");
        if (child == 0) {
            std::atomic<unsigned> executed = 0;
            JournalOptions fault;
            unsigned hits = 0;
            fault.observe = [&](JournalPoint point, std::string_view kind) {
                if (point == JournalPoint::after_sync && kind == boundary && ++hits == 2) ::_exit(11);
            };
            Runtime runtime({"multi"}, epoch(), tools(executed),
                [](const Request& request, std::stop_token) -> Result<CompletedResponse> { return response(request, 3); },
                {.tool_rounds=4, .tool_calls=8}, {temp.path, {}, fault});
            post(runtime, "submit", Submit{"crash in second round"}); until<Done>(runtime); ::_exit(12);
        }
        int status = 0;
        check(::waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 11, "crash boundary");
        std::atomic<unsigned> executed = 0;
        Runtime runtime({"multi"}, epoch(), tools(executed),
            [](const Request& request, std::stop_token) -> Result<CompletedResponse> { return response(request, 3); },
            {.tool_rounds=4, .tool_calls=8}, {temp.path});
        const auto initial = std::get<FrontendSnapshot>(until<FrontendSnapshot>(runtime).payload);
        if (boundary == "intent") {
            check(initial.recovery_operation && !initial.resume_available && executed == 0, "unknown effect replayed");
        } else {
            check(initial.resume_available, "recovery not resumable");
            post(runtime, "resume", Resume{*initial.request}); until<Done>(runtime);
            check(executed == (boundary == "response" ? 2 : 1), "completed rounds reexecuted after crash");
        }
        finish(runtime);
    }
}
void between_rounds() {
    Temp temp; std::atomic<unsigned> executed = 0;
    Runtime* active = nullptr;
    JournalOptions fault; unsigned responses = 0;
    // Obtain the actual request identity from the first accepted event.
    std::optional<RequestId> target;
    fault.observe = [&](JournalPoint point, std::string_view kind) {
        if (point == JournalPoint::after_sync && kind == "response" && ++responses == 2) {
            check(target.has_value(), "missing cancel target"); post(*active, "cancel_between", Cancel{*target});
        }
    };
    std::mutex mutex; std::condition_variable ready; bool released = false;
    Runtime runtime({"multi"}, epoch(), tools(executed),
        [&](const Request& request, std::stop_token) -> Result<CompletedResponse> {
            std::unique_lock lock(mutex); ready.wait(lock, [&] { return released; }); return response(request, 3);
        }, {.tool_rounds=4, .tool_calls=8}, {temp.path, {}, fault});
    active = &runtime;
    post(runtime, "submit", Submit{"cancel second round"});
    target = std::get<CommandAccepted>(until<CommandAccepted>(runtime).payload).request;
    { std::lock_guard lock(mutex); released = true; } ready.notify_all();
    until<Cancelled>(runtime); finish(runtime);
    check(executed == 1, "Cancel after second durable response dispatched another executor");
}
void process_recovery() {
    for (unsigned failure = 0; failure < 3; ++failure) {
        Temp temp;
        auto runner = std::make_shared<ProcessRunner>(temp.root / "operations");
        const ProcessOptions spec{temp.root, {OMH_TEST_SHELL, "-c", "printf x >> marker"}, 3000, 1024};
        auto registry = [&](bool crash_after_execution) {
            ToolRegistry value; auto definition = test_value_tool();
            definition.requires_journal = true;
            definition.invoke = [runner, spec, crash_after_execution](const ToolInvocation& call, std::stop_token stop) {
                auto outcome = runner->run(call, spec, stop);
                if (crash_after_execution) ::_exit(11);
                return outcome;
            };
            definition.recover = [runner](const ToolInvocation& call) { return runner->recover(call); };
            value.register_tool(std::move(definition)); return value.snapshot();
        };
        const auto child = ::fork(); check(child >= 0, "fork");
        if (!child) {
            Runtime runtime({"multi"}, epoch(), registry(true),
                [](const Request& request, std::stop_token) -> Result<CompletedResponse> { return response(request, 1); },
                {}, {temp.path});
            post(runtime, "submit", Submit{"recover process result"}); until<Done>(runtime); ::_exit(12);
        }
        int status = 0; check(::waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 11,
                              "process crash boundary");
        JournalOptions fault;
        fault.observe = [failure](JournalPoint point, std::string_view kind) {
            if (failure && point == JournalPoint::before_sync && kind == "outcome") {
                if (failure == 2) throw std::runtime_error("synthetic recovery write failure");
                fail(ErrorCode::tool, Stage::execution, "synthetic_recovery_write_failure");
            }
        };
        Runtime runtime({"multi"}, epoch(), registry(false),
            [](const Request& request, std::stop_token) -> Result<CompletedResponse> { return response(request, 1); },
            {}, {temp.path, {}, fault});
        const auto initial = std::get<FrontendSnapshot>(until<FrontendSnapshot>(runtime).payload);
        check(initial.tools.size() == 1 && std::holds_alternative<ToolFinished>(initial.tools[0]) &&
              std::get<ToolFinished>(initial.tools[0]).effects == Effects::known_result,
              "known process artifact lost at startup");
        if (failure) {
            check(initial.storage_failed && !initial.durable && !initial.resume_available, "recovery sync failure allowed dispatch");
        } else {
            check(initial.resume_available && initial.durable, "known artifact not resumable");
            post(runtime, "resume", Resume{*initial.request}); until<Done>(runtime);
        }
        finish(runtime);
        check(std::filesystem::file_size(temp.root / "marker") == 1, "process repeated after recovery");
    }
}
}
int main(int argc, char** argv) {
    if (argc == 3 && std::string_view(argv[1]) == "--process-worker") return omh::process_worker(argv[2]);
    try {
        check(argc == 2, "suite required");
        const std::string_view suite = argv[1];
        if (suite == "roundtrip") roundtrip(false);
        else if (suite == "known-failure") roundtrip(true);
        else if (suite == "limit") limit();
        else if (suite == "crash") crash();
        else if (suite == "between-rounds") between_rounds();
        else if (suite == "process-recovery") process_recovery();
        else throw std::runtime_error("unknown suite");
        return 0;
    } catch (const std::exception& error) { std::cerr << error.what() << '\n'; return 1; }
}
