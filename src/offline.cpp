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

Json fixture_call(const std::string& suffix) {
    return Json::object({{"type", Json::string("function_call")}, {"id", Json::string("fixture_item" + suffix)},
        {"call_id", Json::string("fixture_call" + suffix)}, {"namespace", Json::string("omh")},
        {"name", Json::string("get_test_value")}, {"arguments", Json::string("{}")},
        {"status", Json::string("completed")}});
}
Json fixture_reasoning(const std::string& suffix) {
    return Json::object({{"type", Json::string("reasoning")}, {"id", Json::string("fixture_reasoning" + suffix)},
        {"summary", Json::array({})}, {"encrypted_content", Json::string("synthetic_opaque_value" + suffix)}});
}
} // namespace

Result<CompletedResponse> offline_generate(const Request& request, std::stop_token stop) {
    try {
        require(!stop.stop_requested(), "offline_cancelled", Stage::admission, ErrorCode::cancelled);
        const auto& input = request.input();
        const auto& model = request.binding().returned_model;
        const HttpHead head{200, {"text/event-stream"}};
        if (input.empty() || !input.back().has("type") || input.back().text("type") != "function_call_output") {
            const auto suffix = input.size() <= 1 ? std::string{} : "_" + sha256(Json::array(input).dump()).substr(0, 16);
            return decode_offline(request, head, {wire("fixture_response1" + suffix, model,
                                  {fixture_reasoning(suffix), fixture_call(suffix)})},
                                  {}, MimePolicy::strict, stop);
        }

        require(input.size() >= 4, "fixture_replay_conflict", Stage::request);
        const auto& call = input[input.size() - 2];
        const auto call_id = call.text("call_id");
        require(call_id.starts_with("fixture_call"), "fixture_replay_conflict", Stage::request);
        const auto suffix = call_id.substr(std::string("fixture_call").size());
        require(input[input.size() - 3] == fixture_reasoning(suffix) &&
                call == fixture_call(suffix) && input.back().text("call_id") == call_id,
                "fixture_replay_conflict", Stage::request);
        const auto result = Json::parse(input.back().text("output"), 128 * 1024);
        const auto part = Json::object({{"type", Json::string("output_text")},
            {"text", Json::string("OMH_TOOL_OK:" + result.text("value"))}});
        const auto message = Json::object({{"type", Json::string("message")}, {"id", Json::string("fixture_message" + suffix)},
            {"role", Json::string("assistant")}, {"status", Json::string("completed")}, {"content", Json::array({part})}});
        return decode_offline(request, head, {wire("fixture_response2" + suffix, model, {message})}, {}, MimePolicy::strict, stop);
    } catch (const Error& error) {
        return error;
    }
}

Result<CompactedContext> offline_compact(const Request& request, std::stop_token stop) {
    return capture([&] {
        require(!stop.stop_requested(), "offline_cancelled", Stage::admission, ErrorCode::cancelled);
        const auto digest = sha256(Json::array(request.input()).dump());
        std::vector<Json> output;
        for (auto item = request.input().rbegin(); item != request.input().rend(); ++item) {
            if (item->has("role") && item->text("role") == "user") { output.push_back(*item); break; }
        }
        output.push_back(Json::object({{"type", Json::string("compaction")},
            {"id", Json::string("mock_compact_" + digest.substr(0, 16))},
            {"encrypted_content", Json::string("synthetic_compacted_state_" + digest)}}));
        auto result = CompactedContext::admit(request, Json::object({{"object", Json::string("response.compaction")},
            {"id", Json::string("mock_compaction_" + digest.substr(0, 16))}, {"output", Json::array(output)}}));
        if (auto* error = std::get_if<Error>(&result)) throw *error;
        return std::get<CompactedContext>(std::move(result));
    });
}
} // namespace omh
