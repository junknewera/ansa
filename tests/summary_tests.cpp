#include "omh/offline.hpp"
#include "omh/runtime.hpp"
#include "omh/session.hpp"
#include <condition_variable>
#include <iostream>
#include <mutex>
#include <stdexcept>

namespace {
using namespace omh;
void check(bool ok, const char* message) { if (!ok) throw std::runtime_error(message); }
template<class T> T unwrap(Result<T> result) {
    if (const auto* error = std::get_if<Error>(&result)) throw *error;
    return std::get<T>(std::move(result));
}
template<class F> void rejects(F action) {
    try { action(); } catch (const Error&) { return; }
    throw std::runtime_error("expected rejection");
}
std::shared_ptr<const ToolSnapshot> tools() {
    ToolRegistry registry; registry.register_tool(test_value_tool()); return registry.snapshot();
}
ContextEpoch epoch() { return {"summary_test", "Keep original constraints", Json::parse(R"({"constraint":"keep"})")}; }
CompletedResponse answer(const Request& request, const std::string& text, bool refusal = false) {
    return load_response(Json::object({{"request", save_request(request)}, {"id", Json::string("summary_response")},
        {"model", Json::string(request.binding().returned_model)},
        {"items", Json::array({Json::object({{"type", Json::string("message")}, {"id", Json::string("summary_message")},
            {"role", Json::string("assistant")}, {"status", Json::string("completed")},
            {"content", Json::array({Json::object({{"type", Json::string(refusal ? "refusal" : "output_text")},
                {refusal ? "refusal" : "text", Json::string(text)}})})}})})}}));
}
Request history() {
    std::vector<Json> input;
    for (unsigned index = 0; index < 4; ++index) {
        input.push_back(Json::object({{"role", Json::string("user")}, {"content", Json::string("turn_" + std::to_string(index))}}));
        input.push_back(Json::object({{"role", Json::string("assistant")}, {"content", Json::string("reply_" + std::to_string(index))}}));
    }
    return Request({}, epoch(), tools()->advertisement(), input);
}
void codec() {
    const auto original = history(), request = summary_request(original);
    check(request.input().size() == 4 && request.tools().descriptors.empty() &&
          request.json().at("tools").elements().empty() && request.binding() == original.binding(),
          "summary leaked recent turns or tools");
    auto response = answer(request, "Goal ORCHID. Preserve the user's constraints, decisions and remaining work.");
    const auto compacted = unwrap(CompactedContext::from_summary(original, response));
    check(compacted.output().size() == 5, "summary window size");
    for (std::size_t index = 0; index < 4; ++index)
        check(compacted.output()[index + 1] == original.input()[index + 4], "recent turns changed");
    const auto restored = unwrap(CompactedContext::admit(original, compacted.native()));
    check(restored.native() == compacted.native() && compact_epoch(original.epoch(), 1, restored).instructions ==
          original.epoch().instructions, "summary provenance changed");
    rejects([&] { unwrap(CompactedContext::from_summary(original, answer(original, "wrong request"))); });
    rejects([&] { unwrap(CompactedContext::from_summary(original, answer(request, ""))); });
    rejects([&] { unwrap(CompactedContext::from_summary(original, answer(request, "Cannot summarize.", true))); });
    rejects([&] { unwrap(CompactedContext::from_summary(original, answer(request, std::string(16385, 'x')))); });
    auto altered = compacted.output();
    altered.pop_back();
    const auto corrupted = Json::object({{"object", Json::string("omh.summary.v1")},
        {"summary_response", save_response(response)}, {"output", Json::array(altered)}});
    rejects([&] { unwrap(CompactedContext::admit(original, corrupted)); });
}
RuntimeCommand command(const Runtime& run, std::string id, CommandPayload payload, TurnId turn = {"turn"}) {
    return {1, {"test"}, std::move(turn), run.incarnation(), {std::move(id)}, std::move(payload)};
}
void post(Runtime& run, RuntimeCommand value) {
    check(unwrap(run.post(std::move(value))) == PostStatus::accepted, "post");
}
template<class Payload> RuntimeEvent until(Runtime& run) {
    while (auto event = run.next_event()) {
        if (std::holds_alternative<Payload>(event->payload)) return *event;
        if (const auto* error = std::get_if<RuntimeError>(&event->payload)) throw std::runtime_error(error->diagnostic);
    }
    throw std::runtime_error("missing event");
}
void finish(Runtime& run) { run.close_input(); while (run.next_event()) {} }
void runtime_roundtrip() {
    char name[] = "/tmp/omh-summary-XXXXXX";
    const auto temporary = ::mkdtemp(name);
    check(temporary, "mkdtemp");
    const auto path = std::filesystem::path(temporary) / "session";
    unsigned summaries = 0;
    {
        Runtime run({"test"}, epoch(), tools(), offline_generate, {.compact_bytes = 300},
            {path, [&](const Request& source, std::stop_token) {
                ++summaries;
                check(source.input().size() == 15, "unexpected summary source");
                return CompactedContext::from_summary(source, answer(summary_request(source), "Goal ORCHID retained."));
            }, {}, {}, 2});
        for (unsigned index = 0; index < 4; ++index) {
            const auto id = "turn_" + std::to_string(index);
            post(run, command(run, id, Submit{"Call get_test_value. Goal ORCHID."}, {id}));
            until<Done>(run);
        }
        finish(run);
    }
    {
        SessionState restored({"test"}, Request({}, epoch(), tools()->advertisement(), {}));
        Journal journal(path, [&](const Json& record) { restored.apply(record); });
        check(journal.diagnostic().empty() && summaries == 1 && restored.compactions == 1 &&
              restored.history.size() == 16 && restored.history.front().text("content").find("ORCHID") != std::string::npos,
              "prompt compact recovery failed");
    }
    std::filesystem::remove_all(temporary);
}
void cancel_summary() {
    std::mutex mutex;
    std::condition_variable changed;
    bool entered = false, release = false;
    Compact compact = [&](const Request& source, std::stop_token) {
        auto result = CompactedContext::from_summary(source, answer(summary_request(source), "Goal ORCHID."));
        std::unique_lock lock(mutex);
        entered = true; changed.notify_all();
        check(changed.wait_for(lock, std::chrono::seconds(5), [&] { return release; }), "release timeout");
        return result;
    };
    Runtime run({"test"}, epoch(), tools(), offline_generate, {.compact_bytes = 300}, {{}, compact, {}, {}, 2});
    for (unsigned index = 0; index < 3; ++index) {
        const auto id = "turn_" + std::to_string(index);
        post(run, command(run, id, Submit{"Call get_test_value."}, {id}));
        until<Done>(run);
    }
    post(run, command(run, "summary_turn", Submit{"next"}, {"summary_turn"}));
    const auto accepted = until<CommandAccepted>(run);
    {
        std::unique_lock lock(mutex);
        check(changed.wait_for(lock, std::chrono::seconds(5), [&] { return entered; }), "entry timeout");
    }
    post(run, command(run, "cancel", Cancel{*accepted.request}, {"summary_turn"}));
    until<Cancelled>(run);
    {
        std::lock_guard lock(mutex); release = true; changed.notify_all();
    }
    run.close_input();
    std::optional<FrontendSnapshot> last;
    while (const auto event = run.next_event())
        if (const auto* view = std::get_if<FrontendSnapshot>(&event->payload)) last = *view;
    check(last && last->state == "cancelled" && last->compactions == 0 && last->history_items == 15,
          "cancelled summary replaced context");
}
}
int main(int argc, char** argv) {
    try {
        check(argc == 2, "suite required");
        const std::string_view suite = argv[1];
        if (suite == "codec") codec();
        else if (suite == "runtime") runtime_roundtrip();
        else if (suite == "cancel") cancel_summary();
        else throw std::runtime_error("unknown suite");
        return 0;
    } catch (const std::exception& error) { std::cerr << error.what() << '\n'; return 1; }
}
