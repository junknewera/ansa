#include "omh/contracts.hpp"
#include "omh/tools.hpp"
#include "omh/transport.hpp"
#include <algorithm>
#include <fstream>
#include <iostream>
#include <random>
#include <stdexcept>
#include <type_traits>

namespace {
using namespace omh;
std::size_t checks = 0, invocations = 0, rejected = 0;
void check(bool condition, const char* diagnostic) {
    ++checks;
    if (!condition) throw std::runtime_error(diagnostic);
}
template<class T> T unwrap(Result<T> result) {
    if (const auto* error = std::get_if<Error>(&result)) throw *error;
    return std::get<T>(std::move(result));
}
template<class F> void rejects(F&& action) {
    bool caught = false;
    try { action(); } catch (const Error&) { caught = true; }
    check(caught, "expected typed rejection");
}
Json number(std::size_t n) { return Json::parse(std::to_string(n)); }
Json set(const Json& value, const std::string& key, Json replacement) {
    std::vector<std::pair<std::string, Json>> entries;
    for (const auto& k : value.keys()) if (k != key) entries.emplace_back(k, value.at(k.c_str()));
    entries.emplace_back(key, std::move(replacement)); return Json::object(entries);
}
Json without(const Json& value, const std::string& key) {
    std::vector<std::pair<std::string, Json>> entries;
    for (const auto& k : value.keys()) if (k != key) entries.emplace_back(k, value.at(k.c_str()));
    return Json::object(entries);
}
std::string fixture(const char* name = "round1.sse") {
    std::ifstream stream(std::string(OMH_FIXTURES) + "/" + name, std::ios::binary);
    check(bool(stream), "fixture missing");
    return std::string((std::istreambuf_iterator<char>(stream)), {});
}
std::vector<Json> events(const std::string& bytes) {
    std::vector<Json> out;
    SseParser parser([&](SseEvent event) { out.push_back(Json::parse(event.data)); });
    parser.feed(bytes); parser.finish(); return out;
}
std::string wire(const std::vector<Json>& events, bool renumber = true, std::string_view newline = "\n") {
    std::string out;
    for (std::size_t i = 0; i < events.size(); ++i) {
        out += "data: "; out += (renumber ? set(events[i], "sequence_number", number(i)) : events[i]).dump();
        out += newline; out += newline;
    }
    return out;
}
ToolOutcome counted(const Json& args, std::stop_token stop) {
    ++invocations; return test_value_tool().execute(args, stop);
}
ToolOutcome throws(const Json&, std::stop_token) { ++invocations; throw std::runtime_error("private executor detail"); }
bool accepts(const Json&) { return true; }
bool denies(const Json&) { return false; }
ToolOutcome echo(const Json& args, std::stop_token) { ++invocations; return {Execution::succeeded, Effects::known_result, args.dump()}; }
ToolOutcome oversized(const Json&, std::stop_token) { ++invocations; return {Execution::succeeded, Effects::known_result, std::string(128 * 1024 + 1, 'x')}; }
ToolOutcome cancelled_unknown(const Json&, std::stop_token) { ++invocations; return {Execution::cancelled, Effects::unknown, "unknown"}; }
ToolOutcome invalid_utf8(const Json&, std::stop_token) { ++invocations; return {Execution::succeeded, Effects::known_result, "\xc0\xaf"}; }
ToolOutcome inconsistent(const Json&, std::stop_token) { ++invocations; return {Execution::succeeded, Effects::not_started, "bad outcome"}; }
struct Environment { std::shared_ptr<const ToolSnapshot> snapshot; Request request; };
Environment environment(Executor executor = counted, Validator validator = nullptr) {
    ToolRegistry registry;
    auto tool = test_value_tool(); tool.execute = executor;
    if (validator) tool.validate = validator;
    registry.register_tool(std::move(tool));
    auto snapshot = registry.snapshot();
    return {snapshot, Request({}, {"epoch_1", "Exact fixture instructions", Json::parse(R"({"constraints":["offline","pure only"],"root":"fixture","revision":1})")},
        snapshot->advertisement(), {Json::parse(R"({"role":"developer","content":"Fixture constraint"})"),
            Json::parse(R"({"role":"user","content":"Call omh.get_test_value"})")})};
}
CompletedResponse decode(const Environment& env, const std::string& bytes, Limits limits = {}) {
    return unwrap(decode_offline(env.request, {200, {"text/event-stream"}}, {bytes}, limits));
}
std::vector<ToolResult> execute(const Environment& env, const CompletedResponse& response, std::stop_token stop = {}) {
    OperationIds ids("test_run");
    auto prepared = unwrap(PreparedBatch::prepare(response, env.snapshot, ids));
    return unwrap(prepared.execute(stop));
}
void reject_wire(const Environment& env, const std::string& name, const std::string& bytes,
                 Limits limits = {}, HttpHead head = {200, {"text/event-stream"}},
                 MimePolicy policy = MimePolicy::strict, bool semantic = true) {
    const auto before = invocations;
    const auto response = decode_offline(env.request, head, {bytes}, limits, policy);
    if (semantic) check(std::holds_alternative<Error>(response), name.c_str());
    else if (const auto* completed = std::get_if<CompletedResponse>(&response)) {
        OperationIds ids("negative");
        auto batch = PreparedBatch::prepare(*completed, env.snapshot, ids);
        if (auto* prepared = std::get_if<PreparedBatch>(&batch)) { (void)prepared->execute(); check(false, name.c_str()); }
    }
    check(invocations == before, "rejected response invoked executor"); ++rejected;
}
std::vector<Json> two_calls(std::string arguments = "{}") {
    auto es = events(fixture());
    auto call = set(es[7].at("item"), "id", Json::string("fc_b"));
    call = set(call, "call_id", Json::string("call_b")); call = set(call, "arguments", Json::string(arguments));
    const auto terminal = es.back(); es.pop_back();
    es.push_back(Json::object({{"type", Json::string("response.output_item.added")}, {"output_index", number(2)},
        {"item", set(call, "arguments", Json::string(""))}}));
    es.push_back(Json::object({{"type", Json::string("response.output_item.done")}, {"output_index", number(2)}, {"item", call}}));
    auto output = terminal.at("response").at("output").elements(); output.push_back(call);
    es.push_back(set(terminal, "response", set(terminal.at("response"), "output", Json::array(output))));
    return es;
}
void dependencies() {
    const auto info = dependency_smoke();
    check(!info.curl.empty() && !info.yyjson.empty() && !info.openssl.empty() && !info.tls.empty(), "dependency runtime");
    check(info.target == "aarch64-linux" || info.target == "x86_64-linux", "native platform");
    check(sha256("abc") == "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", "EVP SHA256");
    for (const auto& invalid : {"", "{}junk", "{broken}", "[NaN]", "[Infinity]", "[-Infinity]", "[1e9999]",
            "{\"x\":1,\"x\":2}", "{\"a\":1,\"\\u0061\":2}", "{\"a\":{\"x\":1,\"x\":1}}",
            "{\"x\":\"\\ud800\"}", "[1,]", "/*comment*/{}"}) rejects([&] { (void)Json::parse(invalid); });
    rejects([] { (void)Json::parse(std::string("{\"x\":\"") + "\xc0\xaf" + "\"}"); });
    rejects([] { (void)Json::parse("{}", 1); });
    rejects([] { (void)Json::parse("[[[[]]]]", 64, 3); });
    rejects([] { (void)Json::parse("[1,2,3]", 64, 64, 2); });
    for (const char* index : {"true", "false", "-1", "1.0", "1e0", "18446744073709551616", "null", "\"0\""})
        rejects([&] { (void)Json::parse(std::string("{\"i\":") + index + "}").index("i"); });
    check(Json::parse(R"({"i":0})").index("i") == 0, "zero index");
    check(Json::parse(R"({"b":2,"a":1})") == Json::parse(R"({"a":1,"b":2})"), "object semantic equality");
    const std::string precise = R"({"integer":123456789012345678901234567890,"decimal":0.123456789012345678901234567890})";
    check(Json::parse(precise).dump() == precise, "lossless native numeric metadata");
    const auto binary_key = Json::parse(R"({"x":1,"x\u0000y":2})");
    check(binary_key.at(std::string_view("x\0y", 3)) == number(2), "length-aware JSON keys");
    auto value = [] { std::string temporary = R"({"a":[{"text":"λ"}],"opaque":"a\u0000b"})"; return Json::parse(temporary).at("a"); }();
    check(value.elements()[0].text("text") == "λ", "owning JSON lifetime");
    const auto error = Json::parse(diagnostic_json(Error(ErrorCode::protocol, Stage::admission, "fixed_code")).dump());
    check(error.text("diagnostic") == "fixed_code" && error.index("schema_version") == 1, "structured diagnostics");
}
std::vector<SseEvent> parse_sse(const std::vector<std::string>& chunks, Limits limits = {}) {
    std::vector<SseEvent> out;
    SseParser parser([&](SseEvent event) { out.push_back(std::move(event)); }, limits);
    for (const auto& chunk : chunks) parser.feed(chunk);
    parser.finish(); return out;
}
void sse() {
    for (const std::string newline : {"\n", "\r", "\r\n"}) {
        const std::string bytes = "\xef\xbb\xbf: comment" + newline + "event: update" + newline +
            "data: α😀" + newline + "data: second" + newline + newline +
            "data:" + newline + newline + "event: discarded" + newline + newline +
            "data: after reset" + newline + newline;
        for (std::size_t split = 0; split <= bytes.size(); ++split) {
            const auto out = parse_sse({bytes.substr(0, split), bytes.substr(split)});
            check(out.size() == 3 && out[0].name == "update" && out[0].data == "α😀\nsecond" &&
                out[1].name == "message" && out[1].data.empty() && out[2].name == "message", "SSE split/framing/reset");
        }
        std::vector<std::string> chunks;
        for (const char c : bytes) chunks.emplace_back(1, c);
        check(parse_sse(chunks).size() == 3, "SSE bytewise");
    }
    check(parse_sse({"unknown: ignored\nid: 1\nretry: 10\ndata:  two spaces\n\n"})[0].data == " two spaces", "SSE fields");
    for (const std::string bytes : {"data: unclosed", "data: no boundary\n", "event: unfinished\n", "data: x\r", "\xef\xbb", "\xf0\x9f"})
        rejects([&] { (void)parse_sse({bytes}); });
    for (const std::string bad : {"\xc0\xaf", "\xed\xa0\x80", "\xf4\x90\x80\x80", "\x80", "\xe2\x28\xa1", "\xf5\x80\x80\x80"})
        rejects([&] { (void)parse_sse({": " + bad + "\n\n"}); });
    Limits limits;
    limits.line = 4; rejects([&] { (void)parse_sse({"data: x\n\n"}, limits); });
    limits = {}; limits.event = 12; rejects([&] { (void)parse_sse({":123456\n:abcdef\n\n"}, limits); });
    limits = {}; limits.body = 8; rejects([&] { (void)parse_sse({"data:\n\n", ":x\n"}, limits); });
    limits = {}; limits.events = 1; rejects([&] { (void)parse_sse({"data:\n\ndata:\n\n"}, limits); });
    SseParser parser([](SseEvent) {}); parser.finish(); rejects([&] { parser.feed(""); }); rejects([&] { parser.finish(); });
    SseParser poisoned([](SseEvent) { fail(ErrorCode::protocol, Stage::admission, "sink_rejected"); });
    rejects([&] { poisoned.feed("data: x\n\n"); }); rejects([&] { poisoned.finish(); });
    std::mt19937 random(0x0a11ce);
    const auto body = fixture(); const auto env = environment();
    for (unsigned run = 0; run < 32; ++run) {
        std::vector<std::string> chunks;
        for (std::size_t offset = 0; offset < body.size();) {
            const auto size = std::min(body.size() - offset, std::size_t(1 + random() % 47));
            chunks.push_back(body.substr(offset, size)); offset += size;
        }
        check(unwrap(decode_offline(env.request, {200, {"text/event-stream"}}, chunks)).calls().size() == 1, "seeded network partitions");
    }
    for (std::size_t split = 0; split <= body.size(); ++split)
        check(unwrap(decode_offline(env.request, {200, {"text/event-stream"}}, {body.substr(0, split), body.substr(split)})).calls().size() == 1,
              "production all-splits");
}
void reducer() {
    const auto env = environment(); const auto body = fixture(); const auto original = events(body);
    check(decode(env, body).items().size() == 2, "completed native output");
    auto negative = [&](const std::string& name, const std::function<void(std::vector<Json>&)>& mutate) {
        auto es = original; mutate(es); reject_wire(env, name, wire(es));
    };
    const auto metadata = Json::parse(R"({"type":"response.metadata","response_id":"resp_a","metadata":{"openai_chatgpt_moderation_metadata":{}}})");
    auto with_metadata = original;
    with_metadata.insert(with_metadata.begin() + 1, set(metadata, "response_id", original.front().at("response").at("id")));
    check(decode(env, wire(with_metadata)).items() == decode(env, body).items(), "SIWC metadata altered native output");
    auto bad_metadata = with_metadata;
    bad_metadata[1] = set(bad_metadata[1], "response_id", Json::string("wrong"));
    reject_wire(env, "metadata response identity", wire(bad_metadata));
    bad_metadata = with_metadata;
    bad_metadata[1] = without(bad_metadata[1], "response_id");
    reject_wire(env, "metadata missing identity", wire(bad_metadata));
    bad_metadata = with_metadata;
    bad_metadata[1] = set(bad_metadata[1], "metadata", Json::parse("[]"));
    reject_wire(env, "metadata shape", wire(bad_metadata));
    bad_metadata = with_metadata;
    bad_metadata.back() = set(bad_metadata.back(), "type", Json::string("response.failed"));
    reject_wire(env, "metadata never authorizes failed response", wire(bad_metadata));
    negative("orphan argument item", [](auto& e) { e[4] = set(e[4], "item_id", Json::string("orphan")); });
    negative("argument output index", [](auto& e) { e[4] = set(e[4], "output_index", number(0)); });
    negative("argument event call identity", [](auto& e) { e[4] = set(e[4], "call_id", Json::string("other")); });
    negative("argument done name identity", [](auto& e) { e[6] = set(e[6], "name", Json::string("other")); });
    negative("added/done identity", [](auto& e) { e[7] = set(e[7], "item", set(e[7].at("item"), "id", Json::string("other"))); });
    negative("added/done call_id", [](auto& e) { e[7] = set(e[7], "item", set(e[7].at("item"), "call_id", Json::string("other"))); });
    negative("added/done namespace", [](auto& e) { e[7] = set(e[7], "item", set(e[7].at("item"), "namespace", Json::string("other"))); });
    negative("orphan done", [](auto& e) { e[7] = set(e[7], "output_index", number(4)); });
    negative("arguments delta/final", [](auto& e) { e[4] = set(e[4], "delta", Json::string("[]")); });
    negative("arguments done/item", [](auto& e) { e[6] = set(e[6], "arguments", Json::string("{\"x\":1}")); });
    negative("conflicting duplicate done args", [](auto& e) { e.insert(e.begin() + 7, set(e[6], "arguments", Json::string("{\"x\":1}"))); });
    negative("duplicate identical done args", [](auto& e) { e.insert(e.begin() + 7, e[6]); });
    negative("delta after args done", [](auto& e) { e.insert(e.begin() + 7, e[4]); });
    negative("delta after item done", [](auto& e) { e.insert(e.begin() + 8, e[4]); });
    negative("wrong terminal response id", [](auto& e) { e.back() = set(e.back(), "response", set(e.back().at("response"), "id", Json::string("wrong"))); });
    negative("wrong event response id", [](auto& e) { e[4] = set(e[4], "response_id", Json::string("wrong")); });
    negative("wrong created model", [](auto& e) { e[0] = set(e[0], "response", set(e[0].at("response"), "model", Json::string("gpt-other"))); });
    negative("wrong returned model", [](auto& e) { e.back() = set(e.back(), "response", set(e.back().at("response"), "model", Json::string("gpt-other"))); });
    negative("model prefix spoof", [](auto& e) { e[0] = set(e[0], "response", set(e[0].at("response"), "model", Json::string("gpt-6.1-sol-spoof"))); });
    for (const char* index : {"true", "false", "1.0", "-1", "1e0", "18446744073709551616", "\"1\""})
        negative(std::string("invalid index ") + index, [&](auto& e) { e[3] = set(e[3], "output_index", Json::parse(index)); });
    auto seq = original; seq[4] = set(seq[4], "sequence_number", number(3)); reject_wire(env, "duplicate sequence", wire(seq, false));
    seq = original; seq[4] = set(seq[4], "sequence_number", Json::parse("true")); reject_wire(env, "bool sequence", wire(seq, false));
    negative("duplicate added", [](auto& e) { e.insert(e.begin() + 4, e[3]); });
    negative("duplicate item id", [](auto& e) { e[3] = set(e[3], "item", set(e[3].at("item"), "id", Json::string("rs_a"))); });
    negative("duplicate item done", [](auto& e) { e.insert(e.begin() + 8, e[7]); });
    negative("duplicate terminal", [](auto& e) { e.push_back(e.back()); });
    negative("duplicate created", [](auto& e) { e.insert(e.begin() + 1, e[0]); });
    negative("event before created", [](auto& e) { e.erase(e.begin()); });
    negative("unknown event", [](auto& e) { e[4] = set(e[4], "type", Json::string("response.unknown_execution.delta")); });
    for (const char* type : {"custom_tool_call", "local_shell_call", "computer_call", "tool_search_call", "future_execute"})
        negative(std::string("unknown executable ") + type, [&](auto& e) { e[3] = set(e[3], "item", set(e[3].at("item"), "type", Json::string(type))); });
    negative("unknown tool", [](auto& e) { e[3] = set(e[3], "item", set(e[3].at("item"), "name", Json::string("bash"))); });
    negative("missing namespace", [](auto& e) { e[3] = set(e[3], "item", without(e[3].at("item"), "namespace")); });
    negative("missing call id", [](auto& e) { e[3] = set(e[3], "item", without(e[3].at("item"), "call_id")); });
    negative("empty call id", [](auto& e) { e[3] = set(e[3], "item", set(e[3].at("item"), "call_id", Json::string(""))); });
    negative("missing opaque reasoning", [](auto& e) { e[2] = set(e[2], "item", without(e[2].at("item"), "encrypted_content")); });
    negative("empty opaque reasoning", [](auto& e) { e[2] = set(e[2], "item", set(e[2].at("item"), "encrypted_content", Json::string(""))); });
    negative("opaque added/done conflict", [](auto& e) { e[1] = set(e[1], "item", set(e[1].at("item"), "encrypted_content", Json::string("other"))); });
    negative("terminal native order", [](auto& e) { auto out = e.back().at("response").at("output").elements(); std::reverse(out.begin(), out.end()); e.back() = set(e.back(), "response", set(e.back().at("response"), "output", Json::array(out))); });
    negative("missing done item", [](auto& e) { e.erase(e.begin() + 7); });
    negative("noncompleted function", [](auto& e) { e[7] = set(e[7], "item", set(e[7].at("item"), "status", Json::string("in_progress"))); });
    negative("terminal bad status", [](auto& e) { e.back() = set(e.back(), "response", set(e.back().at("response"), "status", Json::string("incomplete"))); });
    negative("completed with error", [](auto& e) { e.back() = set(e.back(), "response", set(e.back().at("response"), "error", Json::parse("{}"))); });
    negative("completed with incomplete details", [](auto& e) { e.back() = set(e.back(), "response", set(e.back().at("response"), "incomplete_details", Json::parse("{}"))); });
    for (const char* type : {"response.failed", "response.incomplete", "error"})
        negative(type, [&](auto& e) { e.back() = set(e.back(), "type", Json::string(type)); });
    negative("EOF before terminal", [](auto& e) { e.pop_back(); });
    for (const auto& [type, code] : std::vector<std::pair<std::string, ErrorCode>>{
            {"response.failed", ErrorCode::response_failed}, {"response.incomplete", ErrorCode::response_incomplete},
            {"eof", ErrorCode::premature_eof}}) {
        auto failed = original;
        if (type == "eof") failed.pop_back(); else failed.back() = set(failed.back(), "type", Json::string(type));
        const auto result = decode_offline(env.request, {200, {"text/event-stream"}}, {wire(failed)});
        check(std::get<Error>(result).code == code, "typed terminal failure state");
    }
    for (const char* bad : {"NaN", "Infinity", "-Infinity", "1e9999"}) {
        auto malformed = body;
        const auto offset = malformed.find("\"sequence_number\":0");
        malformed.insert(offset, std::string("\"metadata\":{\"n\":") + bad + "},");
        reject_wire(env, std::string("nonfinite metadata ") + bad, malformed);
    }
    reject_wire(env, "malformed stream", "data: {broken}\n\n");
    reject_wire(env, "duplicate JSON field", "data: {\"type\":\"response.created\",\"type\":\"response.completed\"}\n\n");
    reject_wire(env, "invalid UTF-8", ": \xc0\xaf\n\n" + body);
    reject_wire(env, "SSE mismatch", "event: response.failed\n" + body);
    reject_wire(env, "trailing partial bytes after terminal", body + "data: {unfinished");
    reject_wire(env, "terminal without blank line", body.substr(0, body.size() - 1));
    reject_wire(env, "event after completed", body + "data: {}\n\n");
    reject_wire(env, "DONE sentinel", body + "data: [DONE]\n\n");
    auto multiple = two_calls(); check(decode(env, wire(multiple)).calls().size() == 2, "multiple functions");
    multiple[8] = set(multiple[8], "item", set(multiple[8].at("item"), "call_id", Json::string("call_a")));
    reject_wire(env, "duplicate call ids", wire(multiple));
    Limits limits; limits.arguments = 1; reject_wire(env, "delta args limit", body, limits);
    auto final_only = original; final_only.erase(final_only.begin() + 4, final_only.begin() + 7);
    limits = {}; limits.arguments = 1; reject_wire(env, "final-only oversized args", wire(final_only), limits);
    check(decode(env, wire(final_only)).calls().size() == 1, "final-only args accepted");
    auto oversized_final = final_only;
    const std::string huge_args = "{\"payload\":\"" + std::string(256 * 1024, 'x') + "\"}";
    oversized_final[4] = set(oversized_final[4], "item", set(oversized_final[4].at("item"), "arguments", Json::string(huge_args)));
    reject_wire(env, "default 256KiB final-only argument limit", wire(oversized_final));
    for (const char* args : {"true", "[]", "{broken}", "{\"x\":NaN}", "{\"a\":1,\"a\":2}"}) {
        auto invalid_args = final_only;
        invalid_args[4] = set(invalid_args[4], "item", set(invalid_args[4].at("item"), "arguments", Json::string(args)));
        reject_wire(env, "invalid final argument JSON", wire(invalid_args));
    }
    limits = {}; limits.calls = 0; reject_wire(env, "call count cap", body, limits);
    limits = {}; limits.items = 1; reject_wire(env, "item count cap", body, limits);
    limits = {}; limits.event = 128; reject_wire(env, "event cap", body, limits);
    limits = {}; limits.body = body.size() - 1; reject_wire(env, "body cap", body, limits);
    std::stop_source stop; ResponseStream pending(env.request, {}, stop.get_token()); pending.feed(body); stop.request_stop();
    check(std::holds_alternative<Error>(pending.finish()), "cancel before admission");
    Binding binding; binding.returned_model = "gpt-6.1-sol-fixture-snapshot";
    Environment bound{env.snapshot, Request(binding, env.request.epoch(), env.request.tools(), env.request.input())};
    auto es = original;
    for (auto& event : es) if (event.has("response")) event = set(event, "response", set(event.at("response"), "model", Json::string(binding.returned_model)));
    const auto completed = decode(bound, wire(es));
    check(completed.returned_model() != completed.request().binding().requested_model &&
        completed.request().json().text("model") == "gpt-6.1-sol", "requested and returned models remain separate");
    const auto text_events = events(fixture("round2.sse"));
    auto text_bad = text_events; text_bad[3] = set(text_bad[3], "item_id", Json::string("orphan"));
    reject_wire(env, "orphan text item", wire(text_bad));
    text_bad = text_events; text_bad[4] = set(text_bad[4], "text", Json::string("conflicting text"));
    reject_wire(env, "text delta/done conflict", wire(text_bad));
    text_bad = text_events; text_bad[5] = set(text_bad[5], "part", set(text_bad[5].at("part"), "text", Json::string("conflicting part")));
    reject_wire(env, "content part final conflict", wire(text_bad));
    text_bad = text_events; text_bad[2] = set(text_bad[2], "content_index", Json::parse("true"));
    reject_wire(env, "boolean content index", wire(text_bad));
    text_bad = text_events; text_bad.back() = set(text_bad.back(), "type", Json::string("response.failed"));
    reject_wire(env, "failed after visible text", wire(text_bad));
    // A failure after a reducer exception cannot be suppressed and then sealed.
    ResponseStream poisoned(env.request);
    rejects([&] { poisoned.feed("data: {broken}\n\n"); });
    check(std::holds_alternative<Error>(poisoned.finish()), "malformed stream remains poisoned");
}
void replay() {
    const auto env = environment(); const auto response = decode(env, fixture());
    const auto results = execute(env, response);
    check(results.size() == 1 && Json::parse(results[0].output).text("value") == "OMH_FIXED_VALUE_42", "production pure tool result");
    const auto second = continuation(response, results);
    // An independent oracle inspects production serialization; it does not implement replay.
    const auto input = Json::parse(second.json().dump()).at("input").elements();
    const auto prefix = env.request.input().size();
    check(input.size() == prefix + response.items().size() + results.size(), "replay oracle: native item count");
    for (std::size_t i = 0; i < prefix; ++i) check(input[i] == env.request.input()[i], "replay oracle: prior input prefix");
    for (std::size_t i = 0; i < response.items().size(); ++i)
        check(input[prefix + i] == response.items()[i], "replay oracle: native item order and values");
    check(input[prefix].text("encrypted_content") == std::string("opaque:v1+/=\0λ", 15), "exact opaque bytes");
    check(input.back().text("call_id") == response.calls()[0].call_id && input.back().text("output") == results[0].output,
          "replay oracle: matching function output");
    check(second.binding() == env.request.binding() && second.epoch().id == env.request.epoch().id &&
        second.epoch().instructions == env.request.epoch().instructions && second.epoch().exact_state == env.request.epoch().exact_state,
        "exact context epoch preserved");
    check(second.json().at("tools") == env.request.json().at("tools") && second.tools().revision == env.request.tools().revision,
        "immutable advertised snapshot");
    auto final = unwrap(decode_offline(second, {200, {"text/event-stream"}}, {fixture("round2.sse")}));
    check(final.calls().empty() && final.items()[0].at("content").elements()[0].text("text") == "OMH_TOOL_OK:OMH_FIXED_VALUE_42",
        "second production response reduction");
    const auto json = second.json();
    check(json.at("store") == Json::parse("false") && json.at("stream") == Json::parse("true") && json.at("input").is_array(), "SIWC profile");
    for (const char* field : {"previous_response_id", "conversation", "temperature", "metadata", "max_output_tokens", "background", "truncation", "user"})
        check(!json.has(field), "SIWC unsupported field");
    check(json.at("tools").elements()[0].text("type") == "namespace" && json.at("tools").elements()[0].text("name") == "omh", "SIWC namespace contract");
    rejects([&] { (void)continuation(response, {}); });
    auto changed = results; changed[0].call_id = "other"; rejects([&] { (void)continuation(response, changed); });
    changed = results; changed[0].effects = Effects::unknown; rejects([&] { (void)continuation(response, changed); });
    changed = results; changed[0].operation_id.clear(); rejects([&] { (void)continuation(response, changed); });
    changed = results; changed[0].output = std::string(128 * 1024 + 1, 'x'); rejects([&] { (void)continuation(response, changed); });
    const auto multi = decode(env, wire(two_calls())); const auto multi_results = execute(env, multi);
    const auto multi_input = continuation(multi, multi_results).json().at("input").elements();
    const auto first_output = env.request.input().size() + multi.items().size();
    check(multi_input[first_output].text("call_id") == "call_a" && multi_input[first_output + 1].text("call_id") == "call_b", "multiple call replay");
    auto reversed = multi_results; std::reverse(reversed.begin(), reversed.end()); rejects([&] { (void)continuation(multi, reversed); });
    reversed = multi_results; reversed[1].operation_id = reversed[0].operation_id; rejects([&] { (void)continuation(multi, reversed); });
    rejects([&] { Request bad({}, env.request.epoch(), env.request.tools(), {Json::parse(R"({"type":"message","role":"system","content":"bad"})")}); });
    Binding wrong; wrong.provider = "other"; rejects([&] { Request bad(wrong, env.request.epoch(), env.request.tools(), {}); });
    wrong = {}; wrong.requested_model.clear(); rejects([&] { Request bad(wrong, env.request.epoch(), env.request.tools(), {}); });
    Binding explicit_model; explicit_model.requested_model = "fixture-model"; explicit_model.returned_model = "fixture-model";
    check(Request(explicit_model, env.request.epoch(), env.request.tools(), {}).json().text("model") == "fixture-model",
          "explicit account-selected model");
}
void tools() {
    const auto env = environment(); const auto response = decode(env, fixture());
    OperationIds ids("run"); auto batch = unwrap(PreparedBatch::prepare(response, env.snapshot, ids));
    const auto before = invocations;
    const auto results = unwrap(batch.execute()); check(invocations == before + 1, "exactly one pure execution");
    check(results[0].operation_id == "run:op:1" && results[0].call_id == "call_a", "runtime assigned operation identity");
    check(std::holds_alternative<Error>(batch.execute()) && invocations == before + 1, "batch reuse rejected");
    auto pending = unwrap(PreparedBatch::prepare(response, env.snapshot, ids)); auto moved = std::move(pending);
    check(std::holds_alternative<Error>(pending.execute()), "moved batch rejected");
    std::stop_source stop; stop.request_stop(); const auto cancelled = unwrap(moved.execute(stop.get_token()));
    check(cancelled[0].execution == Execution::cancelled && cancelled[0].effects == Effects::not_started && invocations == before + 1,
        "cancel before dispatch");
    check(continuation(response, cancelled).input().back().text("call_id") == "call_a", "cancelled call continuity");
    reject_wire(env, "entire batch schema validation", wire(two_calls("{\"unexpected\":true}")), {}, {200, {"text/event-stream"}}, MimePolicy::strict, false);
    const auto validator_env = environment(counted, denies);
    reject_wire(validator_env, "compiled validator rejection", fixture(), {}, {200, {"text/event-stream"}}, MimePolicy::strict, false);
    for (const Executor executor : {throws, oversized, cancelled_unknown, invalid_utf8, inconsistent}) {
        const auto exceptional = environment(executor); const auto complete = decode(exceptional, wire(two_calls()));
        const auto n = invocations; const auto result = execute(exceptional, complete);
        check(result.size() == 2 && result[0].effects == Effects::unknown && result[1].effects == Effects::not_started && invocations == n + 1,
            "post-dispatch failure stops remaining batch with unknown effects");
        rejects([&] { (void)continuation(complete, result); });
    }
    ToolRegistry registry; registry.register_tool(test_value_tool()); auto old = registry.snapshot();
    const auto echo_schema = Json::parse(R"({"type":"object","properties":{"value":{"type":"string"}},"required":["value"],"additionalProperties":false})");
    registry.register_tool({{"echo_test_value", "Echo a fixture value", "1", echo_schema}, accepts, echo});
    const auto added = registry.snapshot();
    check(old->advertisement().descriptors.size() == 1 && added->advertisement().descriptors.size() == 2 &&
        old->advertisement().revision != added->advertisement().revision, "owning immutable registry snapshot");
    check(std::holds_alternative<Error>(PreparedBatch::prepare(response, added, ids)), "different snapshot rejected");
    rejects([&] { registry.register_tool(test_value_tool()); });
    auto invalid = test_value_tool(); invalid.descriptor.schema = set(invalid.descriptor.schema, "additionalProperties", Json::parse("true"));
    rejects([&] { registry.register_tool(invalid); });
    invalid = test_value_tool(); invalid.descriptor.schema = set(invalid.descriptor.schema, "unknownKeyword", Json::parse("true"));
    rejects([&] { registry.register_tool(invalid); });
    // A second pure tool uses exactly the same provider, reducer and batch implementation.
    Environment custom{added, Request({}, env.request.epoch(), added->advertisement(), env.request.input())};
    auto es = events(fixture()); es.erase(es.begin() + 4, es.begin() + 7);
    auto call = set(es[4].at("item"), "name", Json::string("echo_test_value"));
    call = set(call, "arguments", Json::string(R"({"value":"custom fixture"})"));
    es[3] = set(es[3], "item", set(set(call, "status", Json::string("in_progress")), "arguments", Json::string("")));
    es[4] = set(es[4], "item", call);
    auto output = es.back().at("response").at("output").elements(); output[1] = call;
    es.back() = set(es.back(), "response", set(es.back().at("response"), "output", Json::array(output)));
    const auto custom_response = decode(custom, wire(es)); const auto custom_result = execute(custom, custom_response);
    check(Json::parse(custom_result[0].output).text("value") == "custom fixture", "new tool without provider changes");
    check(continuation(custom_response, custom_result).input().back().text("output") == custom_result[0].output, "new tool replay");
}
void transport() {
    const auto env = environment(); const auto body = fixture();
    for (const auto& mime : std::vector<std::vector<std::string>>{{}, {"text/html"}, {"application/json"}, {""},
            {"text/event-stream", "text/event-stream"}, {"text/event-stream, text/html"}, {"text/event-stream; charset=latin1"}})
        reject_wire(env, "invalid MIME", body, {}, {200, mime});
    check(decode_offline(env.request, {200, {}}, {body}, {}, MimePolicy::allow_missing_siwc_responses).index() == 0, "narrow missing MIME opt-in");
    reject_wire(env, "wrong MIME under compatibility", body, {}, {200, {"text/html"}}, MimePolicy::allow_missing_siwc_responses);
    reject_wire(env, "duplicate MIME under compatibility", body, {}, {200, {"text/event-stream", "text/event-stream"}}, MimePolicy::allow_missing_siwc_responses);
    for (const auto status : {301, 401, 429, 500}) reject_wire(env, "HTTP unsuccessful", body, {}, {status, {"text/event-stream"}});
    for (const char* endpoint : {"https://other.example/v1/responses", "http://api.openai.com/v1/responses", "https://api.openai.com/v1/responses?x=1", "https://api.openai.com/v1/models"})
        rejects([&] { admit_http({200, {}}, endpoint, "POST", env.request.json(), MimePolicy::allow_missing_siwc_responses); });
    rejects([&] { admit_http({200, {}}, responses_endpoint, "GET", env.request.json(), MimePolicy::allow_missing_siwc_responses); });
    rejects([&] { admit_http({200, {}}, responses_endpoint, "POST", set(env.request.json(), "stream", Json::parse("false")), MimePolicy::allow_missing_siwc_responses); });
    for (const char* mime : {"text/event-stream", "Text/Event-Stream; charset=UTF-8", "text/event-stream; charset=\"utf-8\""})
        admit_http({200, {mime}}, responses_endpoint, "POST", env.request.json(), MimePolicy::strict);
    HttpHeadParser headers;
    for (const char* line : {"HTTP/1.1 100 Continue\r\n", "X-Ignored: value\r\n", "\r\n", "HTTP/2 200\r\n", "Content-Type: text/event-stream\r\n", "Set-Cookie: synthetic_private_value\r\n", "\r\n"}) headers.line(line);
    check(headers.complete() && headers.head().status == 200 && headers.head().content_types.size() == 1, "interim HTTP header blocks");
    rejects([&] { headers.line("Content-Type: text/html\r\n"); });
    for (const char* line : {"HTTP/2 200x\r\n", "HTTP/2 true\r\n", "HTTP/2 200\n", "Bad Name: x\r\n", "Content-Type: x\r\n"})
        rejects([&] { HttpHeadParser parser; parser.line(line); });
    rejects([] { HttpHeadParser parser; parser.line(std::string(65537, 'x')); });
    HttpHeadParser duplicate; duplicate.line("HTTP/1.1 200 OK\r\n"); duplicate.line("Content-Type: text/event-stream\r\n"); duplicate.line("content-type: text/event-stream\r\n"); duplicate.line("\r\n");
    rejects([&] { admit_http(duplicate.head(), responses_endpoint, "POST", env.request.json(), MimePolicy::strict); });
    std::stop_source stop; stop.request_stop(); std::size_t bytes = 0;
    auto status = CurlTransport{}.post(env.request.json(), "fake-token", [&](std::string_view s) { bytes += s.size(); }, stop.get_token());
    check(std::get<Error>(status).code == ErrorCode::cancelled && bytes == 0, "pre-cancelled curl avoids network");
    for (const char* bearer : {"", "fake\r\nInjected: value", "fake token"}) {
        const auto invalid = CurlTransport{}.post(env.request.json(), bearer, [](std::string_view) {});
        check(std::holds_alternative<Error>(invalid), "credential shape fails before network");
    }
    check(std::holds_alternative<Error>(SiwcProvider{}.generate(env.request, "fake-token", stop.get_token())), "provider cancellation admission");
    check(std::holds_alternative<Error>(decode_offline(env.request, {200, {"text/event-stream"}}, {body}, {}, MimePolicy::strict, stop.get_token())), "offline cancellation");
}
void contracts() {
    static_assert(!std::is_default_constructible_v<CompletedResponse>);
    static_assert(!std::is_copy_constructible_v<PreparedBatch>);
    static_assert(!std::is_same_v<SessionId, TurnId> && !std::is_same_v<RequestId, OperationId>);
    RuntimeCommand command{1, {"session"}, {"turn"}, {"incarnation"}, {"command"}, Submit{"fixture"}}; validate(command);
    command.schema_version = 2; rejects([&] { validate(command); }); command.schema_version = 1;
    command.payload = Cancel{{""}}; rejects([&] { validate(command); });
    command.payload = Cancel{{"request"}}; validate(command);
    command.payload = GetSnapshot{17}; validate(command);
    struct ThrowingText {
        operator std::string() const { throw std::runtime_error("fixture conversion failed"); }
    };
    try { command.payload.emplace<Submit>(ThrowingText{}); } catch (const std::runtime_error&) {}
    if (command.payload.valueless_by_exception()) rejects([&] { validate(command); });
    RuntimeEvent event{1, 18, {"session"}, {"turn"}, {"incarnation"}, {}, FrontendSnapshot{17, "idle"}}; validate(event);
    event.schema_version = 2; rejects([&] { validate(event); }); event.schema_version = 1;
    event.payload = FrontendSnapshot{19, "idle"}; rejects([&] { validate(event); });
    event.payload = ResyncRequired{20}; validate(event);
    event.payload = RecoveryRequired{{"op"}, Effects::unknown}; validate(event);
    event.stream_sequence = 0; rejects([&] { validate(event); });
}
}
int main(int argc, char** argv) {
    try {
        check(argc == 2, "suite required");
        const std::string suite = argv[1];
        if (suite == "dependencies") dependencies();
        else if (suite == "sse") sse();
        else if (suite == "reducer") reducer();
        else if (suite == "replay") replay();
        else if (suite == "tools") tools();
        else if (suite == "transport") transport();
        else if (suite == "contracts") contracts();
        else throw std::runtime_error("unknown suite");
        std::cout << suite << ": " << checks << " checks passed; " << rejected << " rejected responses; " << invocations << " executor invocations\n";
        return 0;
    } catch (const std::exception& error) { std::cerr << error.what() << '\n'; return 1; }
}
