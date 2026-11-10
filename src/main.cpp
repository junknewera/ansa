#include "omh/tools.hpp"
#include "omh/transport.hpp"
#include <iostream>

namespace {
std::string wire(const std::string& response_id, const std::vector<omh::Json>& items) {
    using omh::Json;
    std::vector<Json> events;
    auto lifecycle = [&](const char* type, const char* status, const std::vector<Json>& output) {
        return Json::object({{"type", Json::string(type)}, {"response", Json::object({
            {"id", Json::string(response_id)}, {"model", Json::string("gpt-6.1-sol")},
            {"status", Json::string(status)}, {"output", Json::array(output)}})}});
    };
    events.push_back(lifecycle("response.created", "in_progress", {}));
    for (std::size_t i = 0; i < items.size(); ++i) {
        for (const char* type : {"response.output_item.added", "response.output_item.done"})
            events.push_back(Json::object({{"type", Json::string(type)},
                {"output_index", Json::parse(std::to_string(i))}, {"item", items[i]}}));
    }
    events.push_back(lifecycle("response.completed", "completed", items));
    std::string bytes;
    for (std::size_t i = 0; i < events.size(); ++i) {
        auto event = events[i].dump(); event.pop_back();
        event += ",\"sequence_number\":" + std::to_string(i) + "}";
        bytes += "data: " + event + "\n\n";
    }
    return bytes;
}
template<class T> T unwrap(omh::Result<T> result) {
    if (const auto* error = std::get_if<omh::Error>(&result)) throw *error;
    return std::get<T>(std::move(result));
}
omh::Json demo() {
    using namespace omh;
    ToolRegistry registry; registry.register_tool(test_value_tool());
    const auto snapshot = registry.snapshot();
    Request first({}, {"demo_epoch", "Use the pure fixture tool", Json::parse(R"({"mode":"offline"})")},
        snapshot->advertisement(), {Json::parse(R"({"role":"user","content":"Call omh.get_test_value"})")});
    const auto call = Json::parse(R"({"type":"function_call","id":"fixture_item","call_id":"fixture_call","namespace":"omh","name":"get_test_value","arguments":"{}","status":"completed"})");
    const auto reasoning = Json::parse(R"({"type":"reasoning","id":"fixture_reasoning","summary":[],"encrypted_content":"synthetic_opaque_value"})");
    auto completed = unwrap(decode_offline(first, {200, {"text/event-stream"}}, {wire("fixture_response1", {reasoning, call})}));
    OperationIds ids("demo");
    auto batch = unwrap(PreparedBatch::prepare(completed, snapshot, ids));
    auto results = unwrap(batch.execute());
    const auto second = continuation(completed, results);
    const auto message = Json::parse(R"({"type":"message","id":"fixture_message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"OMH_TOOL_OK:OMH_FIXED_VALUE_42"}]})");
    auto final = unwrap(decode_offline(second, {200, {"text/event-stream"}}, {wire("fixture_response2", {message})}));
    require(final.calls().empty() && second.input().size() == 4 && second.input()[1] == reasoning, "demo_roundtrip");
    return Json::object({{"status", Json::string("passed")}, {"mode", Json::string("offline")},
        {"tool_invocations", Json::parse("1")}, {"native_replay", Json::parse("true")},
        {"final_text", final.items()[0].at("content").elements()[0].at("text")}});
}
} // namespace
int main(int argc, char** argv) {
    try {
        const std::string_view command = argc > 1 ? argv[1] : "--self-check";
        if (argc > 2 || (command != "--self-check" && command != "--offline-demo")) {
            std::cerr << "Usage: omh [--self-check|--offline-demo]\n"; return 2;
        }
        if (command == "--offline-demo") std::cout << demo().dump() << '\n';
        else {
            const auto info = omh::dependency_smoke();
            std::cout << omh::Json::object({{"project", omh::Json::string("oh-my-harness")},
                {"target", omh::Json::string(info.target)}, {"curl", omh::Json::string(info.curl)},
                {"tls", omh::Json::string(info.tls)}, {"yyjson", omh::Json::string(info.yyjson)},
                {"openssl", omh::Json::string(info.openssl)}}).dump() << '\n';
        }
        return 0;
    } catch (const omh::Error& error) { std::cerr << omh::diagnostic_json(error).dump() << '\n'; return 1; }
    catch (...) { std::cerr << "{\"diagnostic\":\"internal_failure\"}\n"; return 1; }
}
