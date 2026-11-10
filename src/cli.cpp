#include "omh/offline.hpp"
#include "omh/runtime.hpp"
#include <chrono>
#include <condition_variable>
#include <iostream>
#include <mutex>
#include <thread>
#include <type_traits>

namespace omh {
namespace {
constexpr std::size_t max_command_line = 128 * 1024;
struct Line { std::string text; bool too_long = false; };

std::optional<Line> read_command_line(std::istream& input) {
    Line line;
    char c;
    while (input.get(c)) {
        if (c == '\n') return line;
        if (line.text.size() < max_command_line) line.text += c;
        else line.too_long = true;
    }
    if (line.text.empty()) return std::nullopt;
    return line;
}

RuntimeCommand decode_command(std::string_view line) {
    const auto json = Json::parse(line, max_command_line, 16, 64);
    require(json.index("schema_version") == 1, "command_version", Stage::frontend, ErrorCode::contract);
    require(json.keys().size() == 7, "command_fields", Stage::frontend, ErrorCode::contract);
    const auto type = json.text("type");
    RuntimeCommand command{1, {json.text("session")}, {json.text("turn")}, {json.text("incarnation")},
                           {json.text("command_id")}, GetSnapshot{0}};
    if (type == "submit") command.payload = Submit{json.text("text")};
    else if (type == "cancel") command.payload = Cancel{{json.text("target")}};
    else if (type == "snapshot") command.payload = GetSnapshot{json.index("after_sequence")};
    else fail(ErrorCode::contract, Stage::frontend, "command_type");
    return command;
}

Json number(std::uint64_t value) { return Json::parse(std::to_string(value)); }

template<class T> Json nullable_id(const std::optional<T>& id) {
    return id ? Json::string(id->value) : Json::parse("null");
}

const char* execution_name(Execution execution) {
    switch (execution) {
        case Execution::succeeded: return "succeeded";
        case Execution::failed: return "failed";
        case Execution::cancelled: return "cancelled";
        case Execution::denied: return "denied";
    }
    std::terminate();
}

const char* effects_name(Effects effects) {
    switch (effects) {
        case Effects::not_started: return "not_started";
        case Effects::known_result: return "known_result";
        case Effects::unknown: return "effect_unknown";
    }
    std::terminate();
}

Json encode_tool_activity(const ToolActivity& tool) {
    return std::visit([](const auto& entry) {
        std::vector<std::pair<std::string, Json>> fields{
            {"operation_id", Json::string(entry.operation.value)}, {"call_id", Json::string(entry.call_id)}};
        if constexpr (std::is_same_v<std::decay_t<decltype(entry)>, ToolFinished>) {
            fields.emplace_back("execution", Json::string(execution_name(entry.execution)));
            fields.emplace_back("effects", Json::string(effects_name(entry.effects)));
            fields.emplace_back("output", Json::string(*entry.output));
        }
        return Json::object(fields);
    }, tool);
}

Json encode_event(const RuntimeEvent& event) {
    std::vector<std::pair<std::string, Json>> fields{
        {"schema_version", number(event.schema_version)}, {"stream_sequence", number(event.stream_sequence)},
        {"session", Json::string(event.session.value)}, {"turn", Json::string(event.turn.value)},
        {"incarnation", Json::string(event.incarnation.value)}, {"request_id", nullable_id(event.request)}};
    auto field = [&](const char* key, Json value) { fields.emplace_back(key, std::move(value)); };
    auto type = [&](const char* name) { field("type", Json::string(name)); };
    if (const auto* snapshot = std::get_if<FrontendSnapshot>(&event.payload)) {
        type("snapshot");
        field("through_sequence", number(snapshot->through_sequence));
        field("state", Json::string(snapshot->state));
        field("worker_busy", Json::parse(snapshot->worker_busy ? "true" : "false"));
        field("completed_tools", number(snapshot->completed_tools));
        field("text", Json::string(snapshot->text));
        field("diagnostic", Json::string(snapshot->diagnostic));
        field("recovery_operation", nullable_id(snapshot->recovery_operation));
        field("submit_id", nullable_id(snapshot->submit));
        field("command_id", nullable_id(snapshot->command));
        std::vector<Json> tools;
        for (const auto& tool : snapshot->tools) tools.push_back(encode_tool_activity(tool));
        field("tools", Json::array(tools));
    } else if (const auto* resync = std::get_if<ResyncRequired>(&event.payload)) {
        type("resync_required");
        field("oldest_available", number(resync->oldest_available));
    } else if (const auto* accepted = std::get_if<CommandAccepted>(&event.payload)) {
        type("command_accepted");
        field("command_id", Json::string(accepted->command.value));
    } else if (const auto* error = std::get_if<RuntimeError>(&event.payload)) {
        type("error");
        field("command_id", nullable_id(error->command));
        field("diagnostic", Json::string(error->diagnostic));
    } else if (const auto* recovery = std::get_if<RecoveryRequired>(&event.payload)) {
        type("recovery_required");
        field("operation_id", Json::string(recovery->operation.value));
        field("effects", Json::string(effects_name(recovery->effects)));
    } else if (const auto* delta = std::get_if<TextDelta>(&event.payload)) {
        type("text_delta");
        field("text", Json::string(delta->text));
    } else if (const auto* started = std::get_if<ToolStarted>(&event.payload)) {
        type("tool_started");
        field("operation_id", Json::string(started->operation.value));
        field("call_id", Json::string(started->call_id));
    } else if (const auto* finished = std::get_if<ToolFinished>(&event.payload)) {
        type("tool_finished");
        field("operation_id", Json::string(finished->operation.value));
        field("call_id", Json::string(finished->call_id));
        field("execution", Json::string(execution_name(finished->execution)));
        field("effects", Json::string(effects_name(finished->effects)));
        field("output", Json::string(*finished->output));
    } else if (const auto* cancelled = std::get_if<Cancelled>(&event.payload)) {
        type("cancelled");
        field("command_id", Json::string(cancelled->command.value));
    } else if (std::holds_alternative<Done>(event.payload)) {
        type("done");
    }
    return Json::object(fields);
}

Result<CompletedResponse> delayed_fixture(const Request& request, std::stop_token stop) {
    // Leave time for the frontend to address Cancel to the active request.
    std::mutex mutex;
    std::condition_variable_any delay;
    std::unique_lock lock(mutex);
    delay.wait_for(lock, stop, std::chrono::milliseconds(500), [] { return false; });
    return offline_generate(request, stop);
}

void post_command(Runtime& runtime, RuntimeCommand command) {
    const auto result = runtime.post(std::move(command));
    if (const auto* error = std::get_if<Error>(&result)) throw *error;
    switch (std::get<PostStatus>(result)) {
        case PostStatus::accepted: return;
        case PostStatus::queue_full: fail(ErrorCode::limit, Stage::frontend, "command_queue_full");
        case PostStatus::closed: fail(ErrorCode::contract, Stage::frontend, "runtime_closed");
        case PostStatus::stale_target: fail(ErrorCode::contract, Stage::frontend, "stale_cancel_target");
        case PostStatus::stale_incarnation: fail(ErrorCode::contract, Stage::frontend, "stale_incarnation");
        case PostStatus::duplicate: fail(ErrorCode::contract, Stage::frontend, "duplicate_command");
        case PostStatus::command_id_limit: fail(ErrorCode::limit, Stage::frontend, "submit_id_limit");
    }
}
} // namespace

int offline_cli() {
    ToolRegistry registry;
    registry.register_tool(test_value_tool());
    Runtime runtime({"offline"}, {"offline_epoch", "Use the pure fixture tool", Json::parse(R"({"mode":"offline"})")},
                    registry.snapshot(), delayed_fixture);
    // Detach both iostream ties and stdio's line-buffered input flush.
    std::ios::sync_with_stdio(false);
    std::cin.tie(nullptr);
    std::cerr.tie(nullptr);
    bool output_failed = false;
    std::jthread output([&](std::stop_token stop) {
        try {
            while (auto event = runtime.next_event(stop)) {
                std::cout << encode_event(*event).dump() << std::endl;
                if (!std::cout) {
                    output_failed = true;
                    return;
                }
            }
        } catch (...) {
            output_failed = true;
        }
    });
    while (auto line = read_command_line(std::cin)) {
        std::optional<CommandId> id;
        try {
            require(!line->too_long, "command_line_limit", Stage::frontend, ErrorCode::limit);
            if (line->text.empty()) continue;
            auto command = decode_command(line->text);
            if (valid_id(command.command.value)) id = command.command;
            post_command(runtime, std::move(command));
        } catch (const Error& error) {
            auto diagnostic = diagnostic_json(error);
            std::vector<std::pair<std::string, Json>> fields{{"type", Json::string("error")},
                                                            {"command_id", nullable_id(id)}};
            for (const auto& key : diagnostic.keys()) fields.emplace_back(key, diagnostic.at(key.c_str()));
            std::cerr << Json::object(fields).dump() << '\n';
        }
    }
    runtime.close_input();
    output.join();
    return output_failed || std::cin.bad() ? 1 : 0;
}
} // namespace omh
