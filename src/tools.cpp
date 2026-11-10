#include "omh/tools.hpp"
#include <algorithm>
#include <limits>
#include <set>

namespace omh {
namespace {
bool name_valid(std::string_view name) {
    return !name.empty() && name.size() <= 64 && std::all_of(name.begin(), name.end(), [](unsigned char c) {
        return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '_';
    });
}
std::vector<std::string> keys(const Json& object) {
    return object.keys();
}
// Deliberately small, closed JSON Schema subset. Compiled validators add domain constraints.
void validate_schema(const Json& schema, unsigned depth = 0) {
    require(schema.is_object() && depth < 16, "tool_schema_shape", Stage::preparation, ErrorCode::validation);
    const auto type = schema.text("type");
    const std::set<std::string> scalars = {"string", "integer", "number", "boolean", "null"};
    require(type == "object" || type == "array" || scalars.contains(type), "tool_schema_type", Stage::preparation);
    std::set<std::string> allowed{"type", "description"};
    if (type == "object") {
        allowed.insert({"properties", "required", "additionalProperties"});
        require(schema.at("properties").is_object() && schema.at("additionalProperties") == Json::parse("false"),
                "tool_schema_strict", Stage::preparation);
        const auto properties = schema.at("properties");
        std::set<std::string> required;
        for (const auto& entry : schema.at("required").elements())
            require(required.insert(entry.text()).second, "tool_schema_required_duplicate", Stage::preparation);
        const auto names = keys(properties);
        require(names.size() <= 64 && required.size() == names.size(), "tool_schema_required", Stage::preparation);
        for (const auto& name : names) {
            require(name_valid(name) && required.contains(name), "tool_schema_property", Stage::preparation);
            validate_schema(properties.at(name.c_str()), depth + 1);
        }
    } else if (type == "array") { allowed.insert("items"); validate_schema(schema.at("items"), depth + 1); }
    for (const auto& key : keys(schema)) require(allowed.contains(key), "unsupported_schema_keyword", Stage::preparation);
    if (schema.has("description")) (void)schema.text("description");
}
bool matches(const Json& schema, const Json& value) {
    const auto type = schema.text("type");
    if (type == "string") return value.is_string();
    if (type == "integer") return value.is_integer();
    if (type == "number") return value.is_number();
    if (type == "boolean") return value.is_boolean();
    if (type == "null") return value.is_null();
    if (type == "array") {
        if (!value.is_array()) return false;
        for (const auto& v : value.elements()) if (!matches(schema.at("items"), v)) return false;
        return true;
    }
    if (!value.is_object()) return false;
    const auto properties = schema.at("properties");
    const auto fields = keys(properties);
    if (keys(value).size() != fields.size()) return false;
    for (const auto& key : fields)
        if (!value.has(key.c_str()) || !matches(properties.at(key.c_str()), value.at(key.c_str()))) return false;
    return true;
}
Json descriptor_json(const ToolDescriptor& d) {
    return Json::object({{"name", Json::string(d.name)}, {"description", Json::string(d.description)},
        {"implementation_revision", Json::string(d.implementation_revision)}, {"schema", d.schema}});
}
bool empty_args(const Json& args) { return args == Json::parse("{}"); }
ToolOutcome fixed_value(const Json&, std::stop_token) {
    return {Execution::succeeded, Effects::known_result, R"({"ok":true,"value":"OMH_FIXED_VALUE_42"})"};
}
} // namespace
ToolSnapshot::ToolSnapshot(std::vector<ToolDefinition> definitions) : definitions_(std::move(definitions)) {
    std::vector<Json> records;
    for (const auto& d : definitions_) {
        advertisement_.descriptors.push_back(d.descriptor);
        records.push_back(descriptor_json(d.descriptor));
    }
    advertisement_.revision = sha256(Json::array(records).dump());
}
void ToolRegistry::register_tool(ToolDefinition definition) {
    require(definitions_.size() < 64 && name_valid(definition.descriptor.name) && definition.validate && definition.execute &&
            !definition.descriptor.implementation_revision.empty() && definition.descriptor.implementation_revision.size() <= 128,
            "invalid_tool_registration", Stage::preparation, ErrorCode::validation);
    require(definition.descriptor.schema.text("type") == "object", "tool_schema_root", Stage::preparation);
    validate_schema(definition.descriptor.schema);
    require(std::none_of(definitions_.begin(), definitions_.end(), [&](const auto& existing) {
        return existing.descriptor.name == definition.descriptor.name;
    }), "duplicate_tool", Stage::preparation, ErrorCode::validation);
    definitions_.push_back(std::move(definition));
}
std::shared_ptr<const ToolSnapshot> ToolRegistry::snapshot() const {
    return std::shared_ptr<const ToolSnapshot>(new ToolSnapshot(definitions_));
}
OperationIds::OperationIds(std::string run) : run_(std::move(run)) {
    require(name_valid(run_), "invalid_run_id", Stage::preparation);
}
std::string OperationIds::next() {
    require(next_ != std::numeric_limits<std::uint64_t>::max(), "operation_id_exhausted", Stage::preparation);
    return run_ + ":op:" + std::to_string(++next_);
}
PreparedBatch::PreparedBatch(std::shared_ptr<const ToolSnapshot> snapshot, std::vector<Prepared> calls)
    : snapshot_(std::move(snapshot)), calls_(std::move(calls)) {}
PreparedBatch::PreparedBatch(PreparedBatch&& other) noexcept
    : snapshot_(std::move(other.snapshot_)), calls_(std::move(other.calls_)), consumed_(other.consumed_) { other.consumed_ = true; }
Result<PreparedBatch> PreparedBatch::prepare(const CompletedResponse& response,
    std::shared_ptr<const ToolSnapshot> snapshot, OperationIds& ids) {
    return capture([&] {
        require(bool(snapshot) && snapshot->advertisement().revision == response.request().tools().revision,
                "snapshot_revision_conflict", Stage::preparation, ErrorCode::validation);
        const auto& advertised = response.request().tools().descriptors;
        require(advertised.size() == snapshot->advertisement().descriptors.size(), "snapshot_descriptor_count", Stage::preparation);
        for (std::size_t i = 0; i < advertised.size(); ++i)
            require(descriptor_json(advertised[i]) == descriptor_json(snapshot->advertisement().descriptors[i]),
                    "snapshot_descriptor_conflict", Stage::preparation);
        std::vector<Prepared> prepared;
        // Validate the ENTIRE batch before allocating identities or invoking an executor.
        for (const auto& call : response.calls()) {
            const auto& defs = snapshot->definitions_;
            const auto it = std::find_if(defs.begin(), defs.end(), [&](const auto& d) { return d.descriptor.name == call.name; });
            require(it != defs.end(), "unknown_tool", Stage::preparation, ErrorCode::validation);
            auto args = Json::parse(call.arguments, 256 * 1024);
            require(matches(it->descriptor.schema, args) && it->validate(args), "tool_arguments_rejected", Stage::preparation, ErrorCode::validation);
            prepared.push_back({{}, call.call_id, std::move(args), static_cast<std::size_t>(it - defs.begin())});
        }
        for (auto& p : prepared) p.operation_id = ids.next();
        return PreparedBatch(std::move(snapshot), std::move(prepared));
    });
}
Result<std::vector<ToolResult>> PreparedBatch::execute(std::stop_token stop) {
    return capture([&] {
        require(!consumed_ && bool(snapshot_), "batch_already_consumed", Stage::execution, ErrorCode::validation);
        consumed_ = true;
        std::vector<ToolResult> results;
        results.reserve(calls_.size()); // No vector allocation after the first dispatch.
        bool blocked = false;
        for (const auto& p : calls_) {
            ToolOutcome outcome{Execution::cancelled, Effects::not_started, R"({"error":"cancelled_before_dispatch"})"};
            if (!blocked && !stop.stop_requested()) {
                try {
                    outcome = snapshot_->definitions_[p.tool].execute(p.args, stop);
                    require(outcome.output.size() <= 128 * 1024, "executor_output_limit", Stage::execution);
                    (void)Json::string(outcome.output); // UTF-8 validation before any replay.
                    require(outcome.execution != Execution::succeeded || outcome.effects == Effects::known_result,
                            "executor_outcome_conflict", Stage::execution);
                } catch (...) {
                    outcome = {Execution::failed, Effects::unknown, R"({"error":"executor_failure","effects":"unknown"})"};
                }
            }
            if (outcome.effects == Effects::unknown) blocked = true;
            results.push_back({p.operation_id, p.call_id, std::move(outcome.output), outcome.execution, outcome.effects});
        }
        return results;
    });
}
ToolDefinition test_value_tool() {
    return {{"get_test_value", "Return a fixed test JSON value without side effects", "1",
        Json::parse(R"({"type":"object","properties":{},"required":[],"additionalProperties":false})")}, empty_args, fixed_value};
}
} // namespace omh
