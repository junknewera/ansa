#pragma once
#include "omh/responses.hpp"

namespace omh {
Result<CompletedResponse> offline_generate(const Request&, std::stop_token = {});
} // namespace omh
