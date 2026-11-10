#include "omh/offline.hpp"
#include "omh/transport.hpp"

namespace omh {
namespace {
std::string wire(const std::string& response_id, const std::string& model, const std::vector<Json>& items) {
    std::string bytes;
    std::size_t sequence = 0;
    auto emit = [&](std::vector<std::pair<std::string, Json>> fields) {
        fields.emplace_back("sequence_number", Json::parse(std::to_string(sequence++)));
        bytes += "data: " + Json::object(fields).dump() + "\n\n";
    };
    auto lifecycle = [&](const char* type, const char* status, const std::vector<Json>& output) {
        emit({{"type", Json::string(type)}, {"response", Json::object({
            {"id", Json::string(response_id)}, {"model", Json::string(model)},
            {"status", Json::string(status)}, {"output", Json::array(output)}})}});
    };
    lifecycle("response.created", "in_progress", {});
    for (std::size_t i = 0; i < items.size(); ++i) {
        for (const char* type : {"response.output_item.added", "response.output_item.done"}) {
            emit({{"type", Json::string(type)}, {"output_index", Json::parse(std::to_string(i))}, {"item", items[i]}});
        }
    }
    lifecycle("response.completed", "completed", items);
    return bytes;
}

Json fixture_call() {
    return Json::parse(R"({"type":"function_call","id":"fixture_item","call_id":"fixture_call","namespace":"omh","name":"get_test_value","arguments":"{}","status":"completed"})");
}
Json fixture_reasoning() {
    return Json::parse(R"({"type":"reasoning","id":"fixture_reasoning","summary":[],"encrypted_content":"synthetic_opaque_value"})");
}
} // namespace

Result<CompletedResponse> offline_generate(const Request& request, std::stop_token stop) {
    try {
        require(!stop.stop_requested(), "offline_cancelled", Stage::admission, ErrorCode::cancelled);
        const auto& input = request.input();
        const auto& model = request.binding().returned_model;
        const HttpHead head{200, {"text/event-stream"}};
        if (input.empty() || !input.back().has("type") || input.back().text("type") != "function_call_output") {
            return decode_offline(request, head, {wire("fixture_response1", model, {fixture_reasoning(), fixture_call()})},
                                  {}, MimePolicy::strict, stop);
        }

        require(input.size() == 4 && input[input.size() - 3] == fixture_reasoning() &&
                input[input.size() - 2] == fixture_call() && input.back().text("call_id") == "fixture_call",
                "fixture_replay_conflict", Stage::request);
        const auto result = Json::parse(input.back().text("output"), 128 * 1024);
        const auto part = Json::object({{"type", Json::string("output_text")},
            {"text", Json::string("OMH_TOOL_OK:" + result.text("value"))}});
        const auto message = Json::object({{"type", Json::string("message")}, {"id", Json::string("fixture_message")},
            {"role", Json::string("assistant")}, {"status", Json::string("completed")}, {"content", Json::array({part})}});
        return decode_offline(request, head, {wire("fixture_response2", model, {message})}, {}, MimePolicy::strict, stop);
    } catch (const Error& error) {
        return error;
    }
}
} // namespace omh
