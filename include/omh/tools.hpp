#pragma once
#include "omh/responses.hpp"
#include <functional>

namespace omh {
struct ToolOutcome { Execution execution; Effects effects; std::string output; };
using Validator = std::function<bool(const Json&)>;
using Executor = std::function<ToolOutcome(const Json&, std::stop_token)>;
struct ToolInvocation {
    std::string operation_id, call_id, name, implementation_revision;
    Json arguments;
};
Json save_invocation(const ToolInvocation&);
ToolInvocation load_invocation(const Json&);
using ContextExecutor = std::function<ToolOutcome(const ToolInvocation&, std::stop_token)>;
using ToolRecovery = std::function<std::optional<ToolOutcome>(const ToolInvocation&)>;
struct ToolDefinition {
    ToolDescriptor descriptor;
    Validator validate;
    Executor execute;
    ContextExecutor invoke{};
    ToolRecovery recover{};
    bool requires_journal = false;
};
struct ToolLease {
    std::shared_ptr<void> ownership;
    std::stop_token stop;
    ToolLease() = default;
    ToolLease(std::shared_ptr<void> value) : ownership(std::move(value)) {}
    ToolLease(std::shared_ptr<void> value, std::stop_token token) : ownership(std::move(value)), stop(token) {}
};
class ToolSnapshot {
    std::vector<ToolDefinition> definitions_;
    ToolAdvertisement advertisement_;
    explicit ToolSnapshot(std::vector<ToolDefinition>);
    friend class ToolRegistry;
    friend class PreparedBatch;
public:
    const ToolAdvertisement& advertisement() const { return advertisement_; }
    bool requires_journal() const;
    std::optional<ToolOutcome> recover(const ToolInvocation&) const;
};
class ToolRegistry {
    std::vector<ToolDefinition> definitions_;
public:
    void register_tool(ToolDefinition);
    std::shared_ptr<const ToolSnapshot> snapshot() const;
};
// Runtime-owned operation ID space, independent of provider call IDs.
class OperationIds {
    std::string run_;
    std::uint64_t next_ = 0;
public:
    explicit OperationIds(std::string run, std::uint64_t offset = 0);
    std::string next();
};
class PreparedBatch {
    struct Prepared { std::string operation_id, call_id; Json args; std::size_t tool; };
    std::shared_ptr<const ToolSnapshot> snapshot_;
    std::vector<Prepared> calls_;
    bool consumed_ = false;
    PreparedBatch(std::shared_ptr<const ToolSnapshot>, std::vector<Prepared>);
public:
    PreparedBatch(const PreparedBatch&) = delete;
    PreparedBatch& operator=(const PreparedBatch&) = delete;
    PreparedBatch(PreparedBatch&&) noexcept;
    PreparedBatch& operator=(PreparedBatch&&) = delete;
    static Result<PreparedBatch> prepare(const CompletedResponse&,
        std::shared_ptr<const ToolSnapshot>, OperationIds&);
    using BeforeStart = std::function<bool(std::string_view operation, std::string_view call)>;
    using AfterFinish = std::function<bool(const ToolResult&)>;
    std::vector<ToolInvocation> plan() const;
    Result<std::vector<ToolResult>> execute(std::stop_token = {}, const BeforeStart& = {},
        const AfterFinish& = {}, const std::vector<ToolResult>& completed = {});
};
ToolDefinition test_value_tool();
} // namespace omh
