#pragma once
#include "omh/responses.hpp"

namespace omh {
Json save_request(const Request&);
Request load_request(const Json&);
Json save_response(const CompletedResponse&);
CompletedResponse load_response(const Json&);
std::string response_text(const CompletedResponse&);
} // namespace omh
