#pragma once
#include "omh/responses.hpp"
#include <functional>

namespace omh {
struct ToolOutcome { Execution execution; Effects effects; std::string output; };
using Validator = bool (*)(const Json&);
using Executor = ToolOutcome (*)(const Json&, std::stop_token);
struct ToolDefinition { ToolDescriptor descriptor; Validator validate; Executor execute; };
struct ToolInvocation {
    std::string operation_id, call_id, name, implementation_revision;
    Json arguments;
};
class ToolSnapshot {
    std::vector<ToolDefinition> definitions_;
    ToolAdvertisement advertisement_;
    explicit ToolSnapshot(std::vector<ToolDefinition>);
    friend class ToolRegistry;
    friend class PreparedBatch;
public:
    const ToolAdvertisement& advertisement() const { return advertisement_; }
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
    explicit OperationIds(std::string run);
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
