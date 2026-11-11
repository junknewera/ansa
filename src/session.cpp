#include "omh/session.hpp"
#include "omh/transport.hpp"

namespace omh {
namespace {
template<class T> T unwrap(Result<T> result) {
    if (auto* error = std::get_if<Error>(&result)) throw *error;
    return std::get<T>(std::move(result));
}
Json number(std::size_t value) { return Json::parse(std::to_string(value)); }
bool same_request(const Request& left, const Request& right) {
    return save_request(left) == save_request(right);
}
std::vector<Json> input_for(const std::vector<Json>& history, const std::string& text) {
    auto input = history;
    input.push_back(Json::object({{"role", Json::string("user")}, {"content", Json::string(text)}}));
    return input;
}
} // namespace

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

Json save_invocation(const ToolInvocation& invocation) {
    return Json::object({{"operation", Json::string(invocation.operation_id)}, {"call", Json::string(invocation.call_id)},
        {"name", Json::string(invocation.name)}, {"revision", Json::string(invocation.implementation_revision)},
        {"arguments", invocation.arguments}});
}

ToolInvocation load_invocation(const Json& value) {
    return {value.text("operation"), value.text("call"), value.text("name"), value.text("revision"), value.at("arguments")};
}

Json save_result(const ToolResult& result) {
    return Json::object({{"operation", Json::string(result.operation_id)}, {"call", Json::string(result.call_id)},
        {"output", Json::string(result.output)}, {"execution", number(static_cast<std::size_t>(result.execution))},
        {"effects", number(static_cast<std::size_t>(result.effects))},
        {"dispatched", Json::parse(result.dispatched ? "true" : "false")}});
}

ToolResult load_result(const Json& value) {
    const auto execution = value.index("execution"), effects = value.index("effects");
    require(execution <= static_cast<std::size_t>(Execution::denied) &&
            effects <= static_cast<std::size_t>(Effects::unknown), "session_outcome");
    auto output = value.text("output");
    require(output.size() <= 128 * 1024, "session_outcome_limit");
    require(value.at("dispatched").is_boolean(), "session_outcome_dispatch");
    const bool dispatched = value.at("dispatched") == Json::parse("true");
    require(dispatched || (effects == static_cast<std::size_t>(Effects::not_started) &&
                          execution != static_cast<std::size_t>(Execution::succeeded)), "session_outcome_dispatch");
    return {value.text("operation"), value.text("call"), std::move(output),
            static_cast<Execution>(execution), static_cast<Effects>(effects), dispatched};
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

SessionState::SessionState(SessionId id, Request config)
    : session(std::move(id)), configuration(std::move(config)), epoch(configuration.epoch()) {}
Json SessionState::initialization() const {
    return Json::object({{"kind", Json::string("init")}, {"version", number(1)},
        {"session", Json::string(session.value)}, {"configuration", save_request(configuration)}});
}
void SessionState::apply(const Json& event) {
    auto next = *this;
    next.reduce(event);
    *this = std::move(next);
}
std::optional<OperationId> SessionState::unknown_operation() const {
    if (!turn) return {};
    if (turn->intent) return OperationId{turn->intent->operation_id};
    for (const auto& result : turn->results)
        if (result.effects == Effects::unknown) return OperationId{result.operation_id};
    return {};
}

void SessionState::reduce(const Json& event) {
    const auto kind = event.text("kind");
    if (kind == "init") {
        require(!initialized && event.index("version") == 1 && event.text("session") == session.value &&
                same_request(load_request(event.at("configuration")), configuration),
                "session_configuration_mismatch", Stage::frontend, ErrorCode::contract);
        initialized = true;
        return;
    }
    require(initialized, "session_missing_init");
    if (kind == "accepted") {
        require(!turn || turn->stage == "completed" || turn->stage == "cancelled", "session_active_turn");
        const auto id = event.text("command"), turn_id = event.text("turn"), request_id = event.text("request");
        const auto text = event.text("text");
        require(valid_id(id) && valid_id(turn_id) && valid_id(request_id) && !text.empty() &&
                text.size() <= max_submit_bytes && !submits.contains(id) &&
                submits.size() < 65536 && event.index("request_number") == next_request + 1,
                "session_submit");
        auto request = load_request(event.at("next"));
        Request expected(configuration.binding(), epoch, configuration.tools(), input_for(history, text));
        require(same_request(request, expected), "session_request_conflict");
        const auto stage = event.at("compact") == Json::parse("true") ? "compacting" : "generating";
        SubmitReceipt receipt{{id}, {turn_id}, {request_id}, std::make_shared<const std::string>(text), stage};
        const auto run = event.text("operation_run");
        OperationIds validate_run(run);
        turn.emplace(SavedTurn{receipt, run, std::move(request), {}, {}, {}, {}, stage, {}, false, {}});
        submits.emplace(id, std::move(receipt));
        ++next_request;
        return;
    }
    require(turn && event.text("turn") == turn->receipt.turn.value, "session_turn_identity");
    auto& current = *turn;
    if (kind == "response") {
        require(current.stage == "generating", "session_response_stage");
        auto response = load_response(event.at("response"));
        auto expected = current.next;
        if (current.tools_response && !current.tools_response->calls().empty()) {
            require(current.results.size() == current.tools_response->calls().size() && !current.intent,
                    "session_results_incomplete");
            expected = continuation(*current.tools_response, current.results);
        }
        require(same_request(response.request(), expected), "session_response_request");
        if (!response.calls().empty()) {
            require(!current.tools_response, "tool_round_limit");
            current.tools_response = response;
            current.stage = "tools";
        } else current.stage = "final";
        current.next = response.request();
        current.response = std::move(response);
    } else if (kind == "intent") {
        require(current.stage == "tools" && current.tools_response && !current.intent &&
                current.results.size() < current.tools_response->calls().size(), "session_intent_stage");
        auto invocation = load_invocation(event.at("invocation"));
        const auto index = current.results.size();
        const auto& call = current.tools_response->calls()[index];
        const auto& descriptors = configuration.tools().descriptors;
        const auto descriptor = std::find_if(descriptors.begin(), descriptors.end(),
            [&](const ToolDescriptor& d) { return d.name == invocation.name; });
        require(invocation.operation_id == current.operation_run + ":op:" + std::to_string(index + 1) &&
                invocation.call_id == call.call_id && invocation.name == call.name &&
                invocation.arguments == Json::parse(call.arguments) && descriptor != descriptors.end() &&
                descriptor->implementation_revision == invocation.implementation_revision, "session_intent_conflict");
        current.intent = std::move(invocation);
    } else if (kind == "outcome") {
        require(current.intent.has_value(), "session_outcome_without_intent");
        auto result = load_result(event.at("result"));
        require(result.operation_id == current.intent->operation_id && result.call_id == current.intent->call_id &&
                (result.execution != Execution::succeeded || result.effects == Effects::known_result),
                "session_outcome_identity");
        current.results.push_back(std::move(result));
        current.intent.reset();
    } else if (kind == "continued") {
        require(current.stage == "tools" && current.tools_response && !current.intent &&
                current.results.size() == current.tools_response->calls().size(), "session_continuation");
        for (const auto& result : current.results)
            require(result.execution == Execution::succeeded && result.effects == Effects::known_result, "session_tool_failed");
        current.stage = "generating";
    } else if (kind == "compacted") {
        require(current.stage == "compacting", "session_compaction_stage");
        const auto value = event.at("compaction");
        auto compacted = unwrap(CompactedContext::admit(load_request(value.at("request")), value.at("native")));
        const Request expected(configuration.binding(), epoch, configuration.tools(), history);
        require(same_request(compacted.request(), expected), "session_compaction_request");
        require(compacted.output().size() + 1 < compact_item_threshold, "compaction_item_limit");
        epoch = compact_epoch(configuration.epoch(), ++compactions, compacted);
        history = compacted.output();
        history_bytes = Json::array(history).dump().size();
        current.next = Request(configuration.binding(), epoch, configuration.tools(), input_for(history, *current.receipt.text));
        current.stage = "generating";
    } else if (kind == "done") {
        require(current.stage == "final" && current.response && current.response->calls().empty(), "session_done_stage");
        (void)response_text(*current.response);
        history = current.response->request().input();
        for (const auto& item : current.response->items()) history.push_back(item);
        history_bytes = Json::array(history).dump().size();
        current.stage = "completed";
    } else if (kind == "cancel") {
        require(current.stage != "completed", "session_cancel_completed");
        current.stage = "cancelled";
        current.paused = false;
    } else if (kind == "pause") {
        require(current.stage != "completed" && current.stage != "cancelled", "session_pause_terminal");
        current.diagnostic = event.text("diagnostic");
        require(valid_id(current.diagnostic), "session_diagnostic");
        current.paused = true;
    } else if (kind == "resume") {
        const auto id = event.text("command");
        require(current.stage != "completed" && current.stage != "cancelled" && !unknown_operation() &&
                valid_id(id) && !current.resumes.contains(id) && current.resumes.size() < 4096,
                "session_resume");
        current.resumes.insert(id);
        current.paused = false;
        current.diagnostic.clear();
    } else fail(ErrorCode::contract, Stage::frontend, "session_record_kind");
    current.receipt.state = current.paused ? "paused" : current.stage;
    submits.at(current.receipt.command.value).state = current.receipt.state;
}
} // namespace omh
