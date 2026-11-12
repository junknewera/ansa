#include "omh/runtime.hpp"
#include "omh/transport.hpp"
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <iostream>
#include <mutex>
#include <thread>
#include <unistd.h>

namespace {
using namespace omh;
using namespace std::chrono_literals;
void check(bool ok, const char* message) { if (!ok) throw std::runtime_error(message); }
template<class T> T unwrap(Result<T> value) { if (auto* e = std::get_if<Error>(&value)) throw *e; return std::get<T>(std::move(value)); }
struct Temp {
    std::filesystem::path root, session;
    Temp() { char path[] = "/tmp/ansa-frontend-XXXXXX"; auto p = ::mkdtemp(path); check(p, "mkdtemp"); root = p; session = root / "session"; }
    ~Temp() { std::error_code e; std::filesystem::remove_all(root, e); }
};
class Gate {
    std::mutex mutex; std::condition_variable cv; bool entered = false, released = false;
public:
    void hold() { std::unique_lock lock(mutex); entered = true; cv.notify_all(); check(cv.wait_for(lock, 10s, [&] { return released; }), "gate timeout"); }
    void wait() { std::unique_lock lock(mutex); check(cv.wait_for(lock, 10s, [&] { return entered; }), "gate entry timeout"); }
    void release() { std::lock_guard lock(mutex); released = true; cv.notify_all(); }
};
Json num(std::size_t n) { return Json::parse(std::to_string(n)); }
std::atomic<unsigned> reads = 0, writes = 0;
std::shared_ptr<const ToolSnapshot> tools() {
    ToolRegistry r;
    for (const auto& [name, readonly] : {std::pair{"safe", true}, {"writer", false}}) {
        ToolDefinition t{{name, "fixture", "1", Json::parse(R"({"type":"object","properties":{},"required":[],"additionalProperties":false})")}, {}, {}};
        t.descriptor = {name, "fixture", "1", Json::parse(R"({"type":"object","properties":{},"required":[],"additionalProperties":false})")};
        t.validate = [](const Json& j) { return !j.has("invalid"); };
        t.execute = [readonly](const Json&, std::stop_token) { (readonly ? reads : writes)++; return ToolOutcome{Execution::succeeded, Effects::known_result, "ok"}; };
        t.read_only = readonly; r.register_tool(std::move(t));
    }
    return r.snapshot();
}
ContextEpoch epoch() { return {"epoch", "fixture", Json::parse("{}")} ; }
CompletedResponse response(const Request& request, std::string text, std::vector<std::pair<std::string, std::string>> calls = {}) {
    std::vector<Json> events, output;
    auto add = [&](std::vector<std::pair<std::string, Json>> fields) {
        fields.emplace_back("sequence_number", num(events.size())); events.push_back(Json::object(fields));
    };
    auto state = [&](std::string status, std::vector<Json> items) { return Json::object({{"id", Json::string("resp")}, {"model", Json::string(request.binding().returned_model)}, {"status", Json::string(status)}, {"output", Json::array(items)}}); };
    add({{"type", Json::string("response.created")}, {"response", state("in_progress", {})}});
    if (!text.empty()) {
        auto initial = Json::parse(R"({"type":"message","id":"msg","role":"assistant","status":"in_progress","content":[]})");
        auto final = Json::object({{"type", Json::string("message")}, {"id", Json::string("msg")}, {"role", Json::string("assistant")}, {"status", Json::string("completed")}, {"content", Json::array({Json::object({{"type", Json::string("output_text")}, {"text", Json::string(text)}})})}});
        add({{"type", Json::string("response.output_item.added")}, {"output_index", num(output.size())}, {"item", initial}});
        add({{"type", Json::string("response.output_item.done")}, {"output_index", num(output.size())}, {"item", final}}); output.push_back(final);
    }
    for (const auto& [name, args] : calls) {
        const auto index = output.size(); auto id = "id" + std::to_string(index); auto call = "call" + std::to_string(index);
        auto item = [&](std::string status, std::string arguments) { return Json::object({{"type", Json::string("function_call")}, {"id", Json::string(id)}, {"call_id", Json::string(call)}, {"name", Json::string(name)}, {"namespace", Json::string("omh")}, {"status", Json::string(status)}, {"arguments", Json::string(arguments)}}); };
        auto final = item("completed", args);
        add({{"type", Json::string("response.output_item.added")}, {"output_index", num(index)}, {"item", item("in_progress", "")}});
        add({{"type", Json::string("response.output_item.done")}, {"output_index", num(index)}, {"item", final}}); output.push_back(final);
    }
    add({{"type", Json::string("response.completed")}, {"response", state("completed", output)}});
    std::string bytes; for (const auto& e : events) bytes += "data: " + e.dump() + "\n\n";
    return unwrap(decode_offline(request, {200, {"text/event-stream"}}, {bytes}));
}
std::unique_ptr<Runtime> make(const std::filesystem::path& path, StreamingGenerate generator, RuntimeLimits limits = {}, JournalOptions journal = {}) {
    SessionOptions options; options.directory = path; options.stream_generate = std::move(generator); options.journal = std::move(journal);
    return std::make_unique<Runtime>(SessionId{"test"}, epoch(), tools(), [](const Request&, std::stop_token) -> Result<CompletedResponse> { throw std::runtime_error("unexpected buffered generation"); }, limits, std::move(options));
}
RuntimeCommand cmd(const Runtime& r, std::string id, CommandPayload p) { return {1, {"test"}, {"turn"}, r.incarnation(), {std::move(id)}, std::move(p)}; }
void post(Runtime& r, std::string id, CommandPayload p, PostStatus expected = PostStatus::accepted) { check(unwrap(r.post(cmd(r, std::move(id), std::move(p)))) == expected, "post status"); }
template<class T, class P> T until(Runtime& r, P predicate) {
    while (auto e = r.next_event()) { validate(*e); if (auto p = std::get_if<T>(&e->payload); p && predicate(*p)) return *p; }
    throw std::runtime_error("missing event");
}
template<class T> T until(Runtime& r) { return until<T>(r, [](const T&) { return true; }); }
RequestId submit(Runtime& r, AgentMode mode = AgentMode::basic) {
    post(r, "submit", Submit{"question", mode});
    std::stop_source timeout;
    std::jthread watchdog([&](std::stop_token stop) {
        std::mutex mutex; std::condition_variable_any cv; std::unique_lock lock(mutex);
        cv.wait_for(lock, stop, 10s, [] { return false; });
        if (!stop.stop_requested()) timeout.request_stop();
    });
    unsigned retry = 0;
    while (auto event = r.next_event(timeout.get_token())) {
        if (auto accepted = std::get_if<CommandAccepted>(&event->payload);
            accepted && accepted->command == CommandId{"submit"} && accepted->request) return *accepted->request;
        if (auto view = std::get_if<FrontendSnapshot>(&event->payload);
            view && view->submit == CommandId{"submit"} && view->request) return *view->request;
        if (std::holds_alternative<ResyncRequired>(event->payload)) {
            const auto status = unwrap(r.post(cmd(r, "submit_snapshot" + std::to_string(retry++), GetSnapshot{0})));
            check(status == PostStatus::accepted || status == PostStatus::queue_full, "submit snapshot post");
        }
    }
    throw std::runtime_error("submit receipt timeout");
}
FrontendSnapshot snapshot(Runtime& r, std::string id) {
    std::stop_source timeout;
    std::jthread watchdog([&](std::stop_token stop) {
        std::mutex mutex; std::condition_variable_any cv; std::unique_lock lock(mutex);
        cv.wait_for(lock, stop, 10s, [] { return false; });
        if (!stop.stop_requested()) timeout.request_stop();
    });
    unsigned attempt = 0;
    auto request = [&] {
        auto status = unwrap(r.post(cmd(r, id + "_" + std::to_string(attempt++), GetSnapshot{0})));
        check(status == PostStatus::accepted || status == PostStatus::queue_full, "snapshot post");
    };
    request();
    while (auto event = r.next_event(timeout.get_token())) {
        if (auto view = std::get_if<FrontendSnapshot>(&event->payload);
            view && view->command && view->command->value.starts_with(id + "_")) return *view;
        if (std::holds_alternative<ResyncRequired>(event->payload)) request();
    }
    throw std::runtime_error("snapshot timeout");
}
TranscriptPage transcript(Runtime& r, std::string id) { post(r, id, GetTranscript{0, 64}); return until<TranscriptPage>(r); }
void drain(Runtime& r) { r.close_input(); while (r.next_event()) {} }
void streaming() {
    Temp temp; Gate gate; std::atomic<int> generations = 0;
    auto gen = [&](const Request& req, std::stop_token, TextPreview preview) -> Result<CompletedResponse> {
        ++generations; preview("hello"); gate.hold(); return response(req, "hello world");
    };
    {
        auto r = make(temp.session, gen); submit(*r); gate.wait();
        const auto update = until<PreviewText>(*r, [](auto& p) { return p.text == "hello"; });
        check(update.state == "preview", "preview not provisional");
        auto view = snapshot(*r, "pending"); check(view.preview && view.preview->text == "hello" && view.text.empty(), "snapshot preview");
        gate.release(); unsigned deltas = 0; bool committed = false;
        while (auto e = r->next_event()) {
            if (auto p = std::get_if<PreviewText>(&e->payload)) committed |= p->state == "committed" && p->text == "hello world";
            if (auto p = std::get_if<TextDelta>(&e->payload)) { ++deltas; check(p->text == "hello world", "final text"); }
            if (std::holds_alternative<Done>(e->payload)) break;
        }
        check(committed && deltas == 1, "commit and one text delta");
        auto page = transcript(*r, "history"); check(page.total == 2 && page.entries[0].role == "user" && page.entries[1].role == "assistant" && page.entries[1].text == "hello world", "durable transcript");
        drain(*r);
    }
    auto r = make(temp.session, gen); auto view = snapshot(*r, "restart");
    check(generations == 1 && !view.preview && view.text == "hello world", "restart preview");
    auto page = transcript(*r, "history2"); check(page.total == 2 && page.entries[1].text == "hello world", "restart transcript"); drain(*r);
}
void recovery() {
    Temp temp; Gate gate; std::atomic<int> attempt = 0;
    auto gen = [&](const Request& req, std::stop_token, TextPreview preview) -> Result<CompletedResponse> {
        if (++attempt == 1) { preview("old"); gate.hold(); preview("old late"); return response(req, "old late"); }
        preview("new"); return response(req, "new");
    };
    auto r = make(temp.session, gen); const auto id = submit(*r); gate.wait();
    until<PreviewText>(*r, [](auto& p) { return p.text == "old"; });
    post(*r, "cancel", Cancel{id}); until<Cancelled>(*r);
    auto view = snapshot(*r, "cancelled"); check(view.text.empty() && view.preview && view.preview->state == "discarded", "cancel snapshot");
    gate.release();
    unsigned n = 0;
    while (snapshot(*r, "wait" + std::to_string(n++)).worker_busy) std::this_thread::sleep_for(1ms);
    auto next = cmd(*r, "second", Submit{"next"}); next.turn = TurnId{"turn2"};
    check(unwrap(r->post(std::move(next))) == PostStatus::accepted, "second generation submit");
    bool late = false;
    while (auto e = r->next_event()) {
        if (auto p = std::get_if<PreviewText>(&e->payload)) late |= p->text == "old late";
        if (std::holds_alternative<Done>(e->payload)) break;
    }
    view = snapshot(*r, "finished"); check(!late && view.preview && view.preview->state == "committed" && view.preview->text == "new", "late preview resurrected");
    drain(*r);
}
void cancel_storage() {
    Temp temp; Gate barrier;
    JournalOptions journal; journal.observe = [&](JournalPoint point, std::string_view kind) {
        if (point == JournalPoint::before_sync && kind == "response") barrier.hold();
    };
    auto gen = [](const Request& req, std::stop_token, TextPreview preview) -> Result<CompletedResponse> {
        preview("visible"); return response(req, "final secret");
    };
    auto r = make(temp.session, gen, {}, journal); auto id = submit(*r); barrier.wait();
    post(*r, "cancel", Cancel{id});
    auto view = snapshot(*r, "during_cancel");
    check(view.state == "cancelled" && view.text.empty(), "cancel exposed nondurable final text");
    barrier.release(); until<Cancelled>(*r); drain(*r);
}
void adversarial() {
    for (int kind = 0; kind < 3; ++kind) {
        Temp temp; reads = 0; writes = 0; Gate gate;
        auto gen = [&](const Request& req, std::stop_token, TextPreview preview) -> Result<CompletedResponse> {
            preview("partial"); gate.hold();
            if (kind == 0) return decode_offline(req, {200, {"text/event-stream"}}, {"data: {broken}\n\n"});
            if (kind == 1) return response(req, "partial", {{"safe", "{}"}, {"writer", R"({"invalid":true})"}});
            return response(req, std::string(65537, 'x'), {{"safe", "{}"}});
        };
        auto r = make(temp.session, gen); submit(*r); gate.wait(); until<PreviewText>(*r, [](auto& p) { return p.text == "partial"; }); gate.release();
        const auto error = until<RuntimeError>(*r); auto view = snapshot(*r, "error");
        if (!(view.state == "paused" && !view.storage_failed && view.preview && view.preview->state == "discarded")) throw std::runtime_error("failure snapshot: " + view.state + " " + view.diagnostic + " " + (view.preview ? view.preview->state : "missing"));
        check(reads == 0 && writes == 0, "invalid batch executed");
        if (kind == 2) check(error.diagnostic == "runtime_text_limit", "oversize diagnostic");
        auto page = transcript(*r, "history"); check(page.total == 1 && page.entries[0].role == "user", "failed assistant transcript"); drain(*r);
    }
}
void failed_done_transcript() {
    Temp temp;
    JournalOptions journal;
    journal.observe = [](JournalPoint point, std::string_view kind) {
        if (point == JournalPoint::before_sync && kind == "done")
            throw Error{ErrorCode::contract, Stage::frontend, "injected_done_failure"};
    };
    auto gen = [](const Request& req, std::stop_token, TextPreview preview) -> Result<CompletedResponse> {
        preview("provisional"); return response(req, "provisional final");
    };
    auto runtime = make(temp.session, gen, {}, journal);
    submit(*runtime);
    until<RuntimeError>(*runtime);
    const auto view = snapshot(*runtime, "failed_done");
    check(view.storage_failed && !view.durable && view.text.empty() && view.transcript_size == 1 &&
          view.preview && view.preview->state == "discarded", "failed done exposed completed response");
    const auto page = transcript(*runtime, "durable_prefix");
    check(page.total == 1 && page.entries.size() == 1 && page.entries.front().role == "user",
          "undurable assistant entered canonical transcript");
    drain(*runtime);
}
void overflow() {
    Temp temp; Gate generation, second_accept;
    std::atomic<unsigned> accepts = 0, generations = 0;
    JournalOptions journal; journal.observe = [&](JournalPoint point, std::string_view kind) {
        if (point == JournalPoint::before_sync && kind == "accepted" && ++accepts == 2) second_accept.hold();
    };
    auto gen = [&](const Request& req, std::stop_token, TextPreview preview) -> Result<CompletedResponse> {
        if (++generations == 1) { preview("part"); generation.hold(); }
        return response(req, "part complete");
    };
    RuntimeLimits limits; limits.events = 2;
    auto r = make(temp.session, gen, limits, journal); submit(*r); generation.wait();
    auto next = cmd(*r, "second", Submit{"next"}); next.turn = TurnId{"turn2"};
    check(unwrap(r->post(std::move(next))) == PostStatus::accepted, "second submit");
    generation.release(); second_accept.wait();
    until<ResyncRequired>(*r);
    second_accept.release();
    unsigned n = 0; auto view = snapshot(*r, "overflow" + std::to_string(n++));
    while (view.state != "completed" || view.submit != CommandId{"second"})
        view = snapshot(*r, "overflow" + std::to_string(n++));
    check(view.state == "completed" && view.preview && view.preview->state == "committed" &&
          view.preview->text == "part complete" && view.text == "part complete", "overflow snapshot");
    auto page = transcript(*r, "history"); check(page.total == 4 && page.entries[1].text == "part complete" && page.entries[3].text == "part complete", "overflow transcript");
    drain(*r);
}
void mode_history() {
    Temp temp; std::atomic<unsigned> count = 0;
    auto gen = [&](const Request& req, std::stop_token, TextPreview) -> Result<CompletedResponse> {
        const auto n = ++count;
        bool writer = false;
        for (const auto& d : req.tools().descriptors) writer |= d.name == "writer";
        if (n == 1) check(writer && req.epoch().id == "epoch" && req.input().size() == 1, "basic initial context");
        else if (n == 2 || n == 3) {
            check(!writer && req.epoch().id != "epoch" && req.input().size() == 3 &&
                  req.input()[0].dump().find("question") != std::string::npos &&
                  req.input()[1].dump().find("answer one") != std::string::npos, "plan shared history");
        } else check(writer && req.epoch().id == "epoch" && req.input().size() == 5 &&
                     req.input()[3].dump().find("answer plan") != std::string::npos, "basic restored history");
        if (n == 2) return decode_offline(req, {200, {"text/event-stream"}}, {"data: {broken}\n\n"});
        return response(req, n == 1 ? "answer one" : n == 3 ? "answer plan" : "answer final");
    };
    {
        auto r = make(temp.session, gen); submit(*r); until<Done>(*r);
        auto plan = cmd(*r, "plan_submit", Submit{"plan question", AgentMode::plan}); plan.turn = TurnId{"plan"};
        check(unwrap(r->post(std::move(plan))) == PostStatus::accepted, "plan submit");
        auto accepted = until<CommandAccepted>(*r, [](auto& x) { return x.command == CommandId{"plan_submit"}; });
        until<RuntimeError>(*r);
        auto paused = snapshot(*r, "plan_pause"); check(paused.mode == AgentMode::plan && paused.state == "paused", "plan pause mode");
        auto resume = cmd(*r, "plan_resume", Resume{*accepted.request}); resume.turn = TurnId{"plan"};
        check(unwrap(r->post(std::move(resume))) == PostStatus::accepted, "plan resume");
        until<Done>(*r);
        auto basic = cmd(*r, "basic_submit", Submit{"last question", AgentMode::basic}); basic.turn = TurnId{"last"};
        check(unwrap(r->post(std::move(basic))) == PostStatus::accepted, "basic submit"); until<Done>(*r);
        auto page = transcript(*r, "mode_history");
        check(page.total == 6 && page.entries[0].mode == AgentMode::basic && page.entries[2].mode == AgentMode::plan &&
              page.entries[3].mode == AgentMode::plan && page.entries[4].mode == AgentMode::basic, "transcript modes");
        drain(*r);
    }
    auto r = make(temp.session, gen); auto view = snapshot(*r, "reopen_mode");
    check(count == 4 && view.mode == AgentMode::basic && view.state == "completed", "reopen mode");
    auto page = transcript(*r, "reopen_history"); check(page.total == 6 && page.entries[3].mode == AgentMode::plan, "reopen transcript modes");
    drain(*r);
}
void modes() {
    Temp temp; reads = 0; writes = 0; std::atomic<int> calls = 0;
    auto gen = [&](const Request& req, std::stop_token, TextPreview) -> Result<CompletedResponse> {
        ++calls;
        bool safe = false, writer = false;
        for (const auto& d : req.tools().descriptors) { safe |= d.name == "safe"; writer |= d.name == "writer"; }
        check(safe && !writer, "plan advertised writer");
        return response(req, "", {{"safe", "{}"}});
    };
    auto r = make(temp.session, gen); submit(*r, AgentMode::plan);
    post(*r, "submit", Submit{"question", AgentMode::basic}, PostStatus::command_conflict);
    until<ToolFinished>(*r); check(reads == 1 && writes == 0, "plan tool dispatch"); drain(*r);
    Temp mixed; reads = 0; writes = 0; Gate mixed_gate;
    auto unsafe = [&](const Request& req, std::stop_token, TextPreview preview) -> Result<CompletedResponse> {
        preview("proposal"); mixed_gate.hold();
        return response(req, "proposal", {{"safe", "{}"}, {"writer", "{}"}});
    };
    auto plan = make(mixed.session, unsafe); submit(*plan, AgentMode::plan); mixed_gate.wait();
    until<PreviewText>(*plan, [](auto& p) { return p.text == "proposal"; }); mixed_gate.release();
    until<RuntimeError>(*plan); auto failed = snapshot(*plan, "mixed");
    check(reads == 0 && writes == 0 && failed.preview && failed.preview->state == "discarded", "plan mixed batch rejected");
    drain(*plan);
    Temp second; reads = 0; writes = 0;
    auto basic = [&](const Request& req, std::stop_token, TextPreview) -> Result<CompletedResponse> { return response(req, "", {{"writer", "{}"}}); };
    auto b = make(second.session, basic); submit(*b); until<ToolFinished>(*b); check(writes == 1, "basic writer"); drain(*b);
}
}
int main(int argc, char** argv) {
    try {
        check(argc == 2, "suite required"); std::string_view suite(argv[1]);
        if (suite == "streaming") streaming(); else if (suite == "recovery") { recovery(); cancel_storage(); }
        else if (suite == "adversarial") { adversarial(); failed_done_transcript(); } else if (suite == "modes") { modes(); mode_history(); }
        else if (suite == "overflow") overflow();
        else throw std::runtime_error("unknown suite");
        return 0;
    } catch (const std::exception& e) { std::cerr << e.what() << '\n'; return 1; }
}
