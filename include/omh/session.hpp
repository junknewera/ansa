#pragma once
#include "omh/compaction.hpp"
#include "omh/contracts.hpp"
#include "omh/tools.hpp"
#include "omh/context_codec.hpp"

namespace omh {
inline constexpr std::size_t compact_item_threshold = max_request_items - 2 * Limits{}.items - Limits{}.calls;
Json save_invocation(const ToolInvocation&);
ToolInvocation load_invocation(const Json&);
Json save_result(const ToolResult&);
ToolResult load_result(const Json&);

struct SubmitReceipt {
    CommandId command;
    TurnId turn;
    RequestId request;
    std::shared_ptr<const std::string> text;
    std::string state;
};
struct SavedTurn {
    SubmitReceipt receipt;
    std::string operation_run;
    Request next;
    std::optional<CompletedResponse> response, tools_response;
    std::vector<ToolResult> results;
    std::optional<ToolInvocation> intent;
    std::string stage, diagnostic;
    bool paused = false;
    std::set<std::string> resumes;
};

// Reducer for the canonical journal. apply() commits an owning state only after
// the complete event has validated; runtime uses the same reducer as recovery.
class SessionState {
    void reduce(const Json&);
public:
    SessionId session;
    Request configuration;
    ContextEpoch epoch;
    std::vector<Json> history;
    std::size_t history_bytes = 2;
    std::map<std::string, SubmitReceipt> submits;
    std::optional<SavedTurn> turn;
    std::size_t next_request = 0, compactions = 0;
    bool initialized = false;
    SessionState(SessionId, Request);
    void apply(const Json&);
    Json initialization() const;
    std::optional<OperationId> unknown_operation() const;
};
} // namespace omh
