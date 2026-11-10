#pragma once
#include "omh/responses.hpp"

namespace omh {
template<class Tag> struct Id {
    std::string value;
    bool operator==(const Id&) const = default;
};
using SessionId = Id<struct SessionTag>;
using TurnId = Id<struct TurnTag>;
using RequestId = Id<struct RequestTag>;
using OperationId = Id<struct OperationTag>;
struct Submit { std::string text; };
struct Cancel { RequestId target; };
struct Resync { std::uint64_t after_sequence; };
using CommandPayload = std::variant<Submit, Cancel, Resync>;
struct RuntimeCommand {
    std::uint32_t schema_version = 1;
    SessionId session;
    TurnId turn;
    CommandPayload payload;
};
struct TextDelta { std::string text; };
struct TurnFinished {};
struct CancelRequested {};
struct RecoveryRequired { OperationId operation; Effects effects = Effects::unknown; };
struct FrontendSnapshot { std::uint64_t through_sequence; std::string state; };
struct ResyncRequired { std::uint64_t oldest_available; };
using EventPayload = std::variant<TextDelta, TurnFinished, CancelRequested, RecoveryRequired,
                                  FrontendSnapshot, ResyncRequired>;
struct RuntimeEvent {
    std::uint32_t schema_version = 1;
    std::uint64_t stream_sequence;
    SessionId session;
    TurnId turn;
    EventPayload payload;
};
inline void validate(const RuntimeCommand& c) {
    require(c.schema_version == 1 && !c.session.value.empty() && !c.turn.value.empty(),
            "command_contract", Stage::frontend, ErrorCode::contract);
    if (const auto* cancel = std::get_if<Cancel>(&c.payload))
        require(!cancel->target.value.empty(), "cancel_target", Stage::frontend, ErrorCode::contract);
}
inline void validate(const RuntimeEvent& e) {
    require(e.schema_version == 1 && e.stream_sequence > 0 && !e.session.value.empty() && !e.turn.value.empty(),
            "event_contract", Stage::frontend, ErrorCode::contract);
    if (const auto* s = std::get_if<FrontendSnapshot>(&e.payload))
        require(s->through_sequence <= e.stream_sequence, "snapshot_sequence", Stage::frontend, ErrorCode::contract);
}
// post/queues, persistence, workers and frontend serialization belong to the next milestone.
} // namespace omh
