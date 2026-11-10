#pragma once
#include "omh/runtime.hpp"
#include <deque>

namespace omh::detail {
struct WorkIdentity {
    RuntimeIncarnation incarnation;
    TurnId turn;
    RequestId request;
    bool operator==(const WorkIdentity&) const = default;
};

struct InitialPrompt { std::string text; OperationIds operations; };
struct RunTools { CompletedResponse response; PreparedBatch batch; };
struct Replay {
    CompletedResponse response;
    std::shared_ptr<const std::vector<ToolResult>> results;
};
using Task = std::variant<InitialPrompt, RunTools, Replay>;
using WorkerResult = std::variant<TextDelta, RunTools, Replay, Error>;
struct Job { WorkIdentity identity; std::stop_token stop; Task task; };
struct Completion { WorkIdentity identity; WorkerResult result; };

enum class Action { wait, stop, tool_started, cancel, complete, snapshot, submit };
struct OwnerInbox {
    std::deque<RuntimeCommand> submits;
    std::optional<RuntimeCommand> cancel, snapshot;
    std::deque<ToolStarted> tool_starts;
    std::optional<Completion> completion;
    bool input_closed = false, aborting = false;

    Action next(bool worker_busy) const {
        if (aborting) return Action::stop;
        // Each of these dispatches already won the lock against post(Cancel).
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
