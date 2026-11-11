#pragma once
#include "omh/compaction.hpp"

namespace omh {
Result<CompletedResponse> offline_generate(const Request&, std::stop_token = {});
Result<CompactedContext> offline_compact(const Request&, std::stop_token = {});
} // namespace omh
