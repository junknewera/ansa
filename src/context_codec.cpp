#include "omh/context_codec.hpp"
#include "omh/transport.hpp"

namespace omh {
namespace {
template<class T> T unwrap(Result<T> result) {
    if (const auto* error = std::get_if<Error>(&result)) throw *error;
    return std::get<T>(std::move(result));
}
Json number(std::size_t value) { return Json::parse(std::to_string(value)); }
}
Json save_request(const Request& request) {
    const auto& binding = request.binding();
    const auto& epoch = request.epoch();
    std::vector<Json> descriptors;
    for (const auto& descriptor : request.tools().descriptors)
        descriptors.push_back(Json::object({{"name", Json::string(descriptor.name)},
            {"description", Json::string(descriptor.description)},
            {"revision", Json::string(descriptor.implementation_revision)}, {"schema", descriptor.schema}}));
    return Json::object({
        {"binding", Json::object({{"provider", Json::string(binding.provider)}, {"account", Json::string(binding.account)},
            {"requested_model", Json::string(binding.requested_model)}, {"returned_model", Json::string(binding.returned_model)}})},
        {"epoch", Json::object({{"id", Json::string(epoch.id)}, {"instructions", Json::string(epoch.instructions)},
            {"exact_state", epoch.exact_state}})},
        {"tools", Json::object({{"revision", Json::string(request.tools().revision)}, {"descriptors", Json::array(descriptors)}})},
        {"input", Json::array(request.input())}});
}

Request load_request(const Json& value) {
    const auto binding = value.at("binding"), epoch = value.at("epoch"), tools = value.at("tools");
    ToolAdvertisement advertisement{tools.text("revision"), {}};
    for (const auto& descriptor : tools.at("descriptors").elements())
        advertisement.descriptors.push_back({descriptor.text("name"), descriptor.text("description"),
                                             descriptor.text("revision"), descriptor.at("schema")});
    return Request({binding.text("provider"), binding.text("account"), binding.text("requested_model"), binding.text("returned_model")},
        {epoch.text("id"), epoch.text("instructions"), epoch.at("exact_state")},
        std::move(advertisement), value.at("input").elements());
}

Json save_response(const CompletedResponse& response) {
    return Json::object({{"request", save_request(response.request())}, {"id", Json::string(response.id())},
        {"model", Json::string(response.returned_model())}, {"items", Json::array(response.items())}});
}

CompletedResponse load_response(const Json& value) {
    auto request = load_request(value.at("request"));
    const auto items = value.at("items").elements();
    std::string wire;
    std::size_t sequence = 0;
    auto emit = [&](std::vector<std::pair<std::string, Json>> fields) {
        fields.emplace_back("sequence_number", number(sequence++));
        wire += "data: " + Json::object(fields).dump() + "\n\n";
    };
    auto lifecycle = [&](const char* type, const char* status, const std::vector<Json>& output) {
        emit({{"type", Json::string(type)}, {"response", Json::object({{"id", value.at("id")}, {"model", value.at("model")},
            {"status", Json::string(status)}, {"output", Json::array(output)}})}});
    };
    lifecycle("response.created", "in_progress", {});
    for (std::size_t index = 0; index < items.size(); ++index)
        for (const char* type : {"response.output_item.added", "response.output_item.done"})
            emit({{"type", Json::string(type)}, {"output_index", number(index)}, {"item", items[index]}});
    lifecycle("response.completed", "completed", items);
    Limits limits;
    limits.body = 128 * 1024 * 1024;
    limits.event = limits.line = 32 * 1024 * 1024;
    return unwrap(decode_offline(request, {200, {"text/event-stream"}}, {wire}, limits));
}

std::string response_text(const CompletedResponse& response) {
    std::string text;
    for (const auto& item : response.items()) {
        if (item.text("type") != "message") continue;
        for (const auto& part : item.at("content").elements()) {
            const auto chunk = part.text(part.text("type") == "refusal" ? "refusal" : "text");
            require(chunk.size() <= 64 * 1024 - text.size(), "runtime_text_limit", Stage::frontend, ErrorCode::limit);
            text += chunk;
        }
    }
    return text;
}

} // namespace omh
