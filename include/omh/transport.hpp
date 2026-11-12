#pragma once
#include "omh/responses.hpp"

namespace omh {
inline constexpr std::string_view responses_endpoint = "https://api.openai.com/v1/responses";
struct HttpHead { long status = 0; std::vector<std::string> content_types; };
struct HttpRequest {
    std::string method, url, body, content_type, bearer;
    std::size_t body_limit = 2 * 1024 * 1024;
    long timeout_ms = 20000;
};
struct HttpReply { HttpHead head; std::string body; };
using HttpExchange = std::function<HttpReply(const HttpRequest&, std::stop_token)>;
HttpReply http_exchange(const HttpRequest&, std::stop_token = {});
class HttpHeadParser {
    HttpHead head_;
    std::size_t bytes_ = 0, fields_ = 0;
    bool complete_ = false, final_ = false;
public:
    void line(std::string_view); // libcurl delivers complete header lines, including status/trailers.
    const HttpHead& head() const { return head_; }
    bool complete() const { return complete_; }
};
enum class MimePolicy { strict, allow_missing_siwc_responses };
void admit_http(const HttpHead&, std::string_view endpoint, std::string_view method,
                const Json& request, MimePolicy);
struct DependencyInfo { std::string curl, tls, yyjson, openssl, target; };
DependencyInfo dependency_smoke();
// Byte transport only. The caller may seal ResponseStream only after successful transport.
class CurlTransport {
public:
    Result<std::monostate> post(const Json& request, std::string_view bearer,
        const std::function<void(std::string_view)>& bytes, std::stop_token = {},
        MimePolicy = MimePolicy::strict) const;
};
class SiwcProvider {
public:
    Result<CompletedResponse> generate(const Request&, std::string_view bearer,
        std::stop_token = {}, MimePolicy = MimePolicy::strict, TextPreview = {}) const;
};
Result<CompletedResponse> decode_offline(const Request&, const HttpHead&,
    const std::vector<std::string>& chunks, Limits = {}, MimePolicy = MimePolicy::strict,
    std::stop_token = {}, TextPreview = {});
} // namespace omh
