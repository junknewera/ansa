#pragma once
#include "omh/compaction.hpp"
#include "omh/contracts.hpp"
#include "omh/tools.hpp"
#include "omh/context_codec.hpp"

namespace omh {
inline constexpr std::size_t compact_item_threshold = max_request_items - 2 * Limits{}.items - Limits{}.calls;
Json save_result(const ToolResult&);
ToolResult load_result(const Json&);

struct SubmitReceipt {
    CommandId command;
    TurnId turn;
    RequestId request;
    std::shared_ptr<const std::string> text;
    std::string state;
    AgentMode mode = AgentMode::basic;
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
    std::vector<ToolResult> previous_results{};
    std::size_t rounds = 0, operation_offset = 0;
    std::size_t round_limit = 1, call_limit = 16;
    std::size_t resume_round = 0, resume_operation = 0;
};

// Persistent blocks keep snapshots cheap while preserving owning entries.
class TranscriptIndex {
    struct Block {
        std::shared_ptr<const Block> previous;
        std::vector<std::shared_ptr<const TranscriptEntry>> entries;
    };
    std::shared_ptr<const Block> tail_;
    std::size_t size_ = 0;
    friend class SessionState;
    void append(TranscriptEntry);
    void set_last_state(const std::string&);
    const TranscriptEntry& back() const;
public:
    std::size_t size() const { return size_; }
    TranscriptPage page(GetTranscript) const;
};

// Reducer for the canonical journal. apply() commits an owning state only after
// the complete event has validated; runtime uses the same reducer as recovery.
class SessionState {
    void reduce(const Json&);
    ToolAdvertisement plan_tools_;
    std::shared_ptr<const TranscriptIndex> transcript_ = std::make_shared<const TranscriptIndex>();
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
    SessionState(SessionId, Request, ToolAdvertisement plan_tools = {});
    Request request_for(const std::string&, AgentMode mode = AgentMode::basic) const;
    std::shared_ptr<const TranscriptIndex> transcript() const { return transcript_; }
    TranscriptPage transcript_page(GetTranscript) const;
    void apply(const Json&);
    Json initialization() const;
    std::optional<OperationId> unknown_operation() const;
};
} // namespace omh
