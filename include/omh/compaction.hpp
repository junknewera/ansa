#pragma once
#include "omh/responses.hpp"
#include <functional>

namespace omh {
class CompactedContext {
    Request request_;
    Json native_;
    CompactedContext(Request, Json);
public:
    static Result<CompactedContext> admit(Request, Json);
    const Request& request() const { return request_; }
    const Json& native() const { return native_; }
    std::vector<Json> output() const { return native_.at("output").elements(); }
};
using Compact = std::function<Result<CompactedContext>(const Request&, std::stop_token)>;
ContextEpoch compact_epoch(const ContextEpoch& base, std::size_t generation, const CompactedContext&);
} // namespace omh
