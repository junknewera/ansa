#pragma once
#include "omh/responses.hpp"
#include <algorithm>

namespace omh {
template<class Tag> struct Id {
    std::string value;
    bool operator==(const Id&) const = default;
};
using SessionId = Id<struct SessionTag>;
using TurnId = Id<struct TurnTag>;
using RequestId = Id<struct RequestTag>;
using OperationId = Id<struct OperationTag>;
using CommandId = Id<struct CommandTag>;
using RuntimeIncarnation = Id<struct RuntimeIncarnationTag>;
struct Submit { std::string text; };
struct Cancel { RequestId target; };
struct GetSnapshot { std::uint64_t after_sequence; };
using CommandPayload = std::variant<Submit, Cancel, GetSnapshot>;
struct RuntimeCommand {
    std::uint32_t schema_version = 1;
    SessionId session;
    TurnId turn;
    RuntimeIncarnation incarnation;
    CommandId command;
    CommandPayload payload;
};
struct CommandAccepted { CommandId command; std::optional<RequestId> request; };
struct TextDelta { std::string text; };
struct ToolStarted { OperationId operation; std::string call_id; };
struct ToolFinished {
    OperationId operation;
    std::string call_id;
    Execution execution;
    Effects effects;
    std::shared_ptr<const std::string> output;
};
using ToolActivity = std::variant<ToolStarted, ToolFinished>;
struct Cancelled { CommandId command; };
struct Done {};
struct RuntimeError { std::optional<CommandId> command; std::string diagnostic; };
struct RecoveryRequired { OperationId operation; Effects effects = Effects::unknown; };
struct FrontendSnapshot {
    std::uint64_t through_sequence;
    std::string state;
    std::optional<RequestId> request;
    bool worker_busy = false;
    std::size_t completed_tools = 0;
    std::string text, diagnostic;
    std::optional<OperationId> recovery_operation;
    std::optional<CommandId> submit;
    std::optional<CommandId> command;
    std::vector<ToolActivity> tools;
};
struct ResyncRequired { std::uint64_t oldest_available; };
using EventPayload = std::variant<CommandAccepted, TextDelta, ToolStarted, ToolFinished, Cancelled, Done,
                                  RuntimeError, RecoveryRequired, FrontendSnapshot, ResyncRequired>;
struct RuntimeEvent {
    std::uint32_t schema_version = 1;
    std::uint64_t stream_sequence;
    SessionId session;
    TurnId turn;
    RuntimeIncarnation incarnation;
    std::optional<RequestId> request;
    EventPayload payload;
};
inline constexpr std::size_t max_submit_bytes = 16 * 1024;
inline bool valid_id(std::string_view value) {
    return !value.empty() && value.size() <= 64 && std::all_of(value.begin(), value.end(), [](unsigned char c) {
        return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '_' || c == '-';
    });
}
inline void validate(const RuntimeCommand& c) {
    require(c.schema_version == 1 && valid_id(c.session.value) && valid_id(c.turn.value) &&
            valid_id(c.incarnation.value) && valid_id(c.command.value) && !c.payload.valueless_by_exception(),
            "command_contract", Stage::frontend, ErrorCode::contract);
    if (const auto* cancel = std::get_if<Cancel>(&c.payload))
        require(valid_id(cancel->target.value), "cancel_target", Stage::frontend, ErrorCode::contract);
    if (const auto* submit = std::get_if<Submit>(&c.payload)) {
        require(!submit->text.empty() && submit->text.size() <= max_submit_bytes,
                "submit_size", Stage::frontend, ErrorCode::limit);
        (void)Json::string(submit->text);
    }
}
inline void validate(const RuntimeEvent& e) {
    require(e.schema_version == 1 && e.stream_sequence > 0 && valid_id(e.session.value) &&
            valid_id(e.turn.value) && valid_id(e.incarnation.value) && (!e.request || valid_id(e.request->value)) &&
            !e.payload.valueless_by_exception(),
            "event_contract", Stage::frontend, ErrorCode::contract);
    if (const auto* s = std::get_if<FrontendSnapshot>(&e.payload))
        require(s->through_sequence <= e.stream_sequence, "snapshot_sequence", Stage::frontend, ErrorCode::contract);
}
} // namespace omh
