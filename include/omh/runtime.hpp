#pragma once
#include "omh/contracts.hpp"
#include "omh/tools.hpp"
#include "omh/compaction.hpp"
#include "omh/journal.hpp"

namespace omh {
using Generate = std::function<Result<CompletedResponse>(const Request&, std::stop_token)>;
enum class PostStatus { accepted, queue_full, closed, stale_target, stale_incarnation, duplicate, command_id_limit,
                        command_conflict };
struct RuntimeLimits {
    std::size_t commands = 16;
    std::size_t events = 64;
    std::size_t submit_ids = 4096;
    std::size_t compact_bytes = 256 * 1024;
};
struct SessionOptions {
    std::filesystem::path directory;
    Compact compact;
    JournalOptions journal;
    Binding binding;
    std::size_t keep_recent_turns = 0;
    std::function<void(std::uint64_t resume_epoch, std::stop_token)> check_dispatch;
    std::function<std::shared_ptr<void>(std::stop_token)> tool_guard;
};

// One session, one turn at a time. Submits wait in FIFO order while the worker is busy.
// Worker and stop callbacks must cooperate with shutdown.
class Runtime {
    class Impl;
    std::unique_ptr<Impl> impl_;
public:
    Runtime(SessionId, ContextEpoch, std::shared_ptr<const ToolSnapshot>, Generate, RuntimeLimits = {}, SessionOptions = {});
    ~Runtime();
    Runtime(const Runtime&) = delete;
    Runtime& operator=(const Runtime&) = delete;

    const RuntimeIncarnation& incarnation() const;
    Result<PostStatus> post(RuntimeCommand);
    std::optional<RuntimeEvent> next_event(std::stop_token = {});
    // Finish accepted commands and drain the outbox. Destruction requests stop instead.
    void close_input();
};
} // namespace omh
