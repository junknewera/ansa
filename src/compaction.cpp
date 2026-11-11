#include "omh/compaction.hpp"

namespace omh {
CompactedContext::CompactedContext(Request request, Json native)
    : request_(std::move(request)), native_(std::move(native)) {}

Result<CompactedContext> CompactedContext::admit(Request request, Json native) {
    return capture([&] {
        require(native.is_object() && native.text("object") == "response.compaction" &&
                !native.text("id").empty(), "compaction_contract", Stage::admission);
        auto output = native.at("output").elements();
        require(!output.empty() && output.size() <= 4096, "compaction_output_limit", Stage::admission);
        std::size_t compactions = 0;
        for (const auto& item : output) {
            require(item.is_object(), "compaction_item", Stage::admission);
            if (item.has("type") && item.text("type") == "compaction") {
                require(++compactions == 1 && !item.text("id").empty() &&
                        !item.text("encrypted_content").empty(), "compaction_item", Stage::admission);
            } else {
                require(item.text("role") == "user" && item.at("content").is_string(),
                        "compaction_retained_item", Stage::admission);
            }
        }
        require(compactions == 1 && native.dump().size() <= 4 * 1024 * 1024,
                "compaction_output_limit", Stage::admission);
        return CompactedContext(std::move(request), std::move(native));
    });
}

ContextEpoch compact_epoch(const ContextEpoch& base, std::size_t generation, const CompactedContext& compacted) {
    return {"compact_" + std::to_string(generation) + "_" + sha256(compacted.native().dump()).substr(0, 16),
        base.instructions, Json::object({{"base", base.exact_state}, {"base_id", Json::string(base.id)},
            {"compaction_generation", Json::parse(std::to_string(generation))},
            {"compaction_digest", Json::string(sha256(compacted.native().dump()))}})};
}
} // namespace omh
