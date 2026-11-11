#pragma once
#include "omh/runtime.hpp"
#include <deque>

namespace omh::detail {
struct WorkIdentity {
    RuntimeIncarnation incarnation;
    TurnId turn;
    RequestId request;
    std::uint64_t resume_epoch = 0;
    bool operator==(const WorkIdentity&) const = default;
};
struct GenerateRequest { Request request; };
struct PrepareTools {
    CompletedResponse response;
    std::string operation_run;
    std::vector<ToolResult> completed;
    std::size_t operation_offset = 0;
};
struct RunTools {
    CompletedResponse response;
    PreparedBatch batch;
    std::vector<ToolResult> completed;
};
struct Replay {
    CompletedResponse response;
    std::shared_ptr<const std::vector<ToolResult>> results;
    std::optional<Error> dispatch_error{};
};
struct CompactRequest { Request request; };
using Task = std::variant<GenerateRequest, PrepareTools, RunTools, Replay, CompactRequest>;
using WorkerResult = std::variant<CompletedResponse, RunTools, Replay, CompactedContext, Error>;
struct Job { WorkIdentity identity; std::stop_token stop; Task task; };
struct Completion { WorkIdentity identity; WorkerResult result; };

enum class Action { wait, stop, tool_started, cancel, complete, snapshot, submit };
struct OwnerInbox {
    std::deque<RuntimeCommand> submits;
    std::optional<RuntimeCommand> cancel, snapshot, resume;
    std::deque<CommandId> lookups;
    std::deque<ToolStarted> tool_starts;
    std::optional<Completion> completion;
    bool input_closed = false, aborting = false;

    Action next(bool worker_busy) const {
        if (aborting) return Action::stop;
        if (!tool_starts.empty()) return Action::tool_started;
        if (cancel) return Action::cancel;
        if (completion) return Action::complete;
        if (snapshot) return Action::snapshot;
        if (!worker_busy && !submits.empty()) return Action::submit;
        if (!worker_busy && input_closed) return Action::stop;
        return Action::wait;
    }
};
} // namespace omh::detail
