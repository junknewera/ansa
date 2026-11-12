#include "omh/transport.hpp"
#include <curl/curl.h>
#include <openssl/crypto.h>
#include <algorithm>
#include <charconv>
#include <limits>

namespace omh {
namespace {
struct CurlGlobal {
    CurlGlobal() { require(curl_global_init(CURL_GLOBAL_DEFAULT) == CURLE_OK, "curl_global_init", Stage::http, ErrorCode::transport); }
    ~CurlGlobal() { curl_global_cleanup(); }
};
void initialize() { static CurlGlobal global; }
using Easy = std::unique_ptr<CURL, decltype(&curl_easy_cleanup)>;
using Headers = std::unique_ptr<curl_slist, decltype(&curl_slist_free_all)>;
std::string lower(std::string_view s) {
    std::string out(s);
    for (char& c : out) if (c >= 'A' && c <= 'Z') c = static_cast<char>(c + ('a' - 'A'));
    return out;
}
std::string_view trim(std::string_view s) {
    while (!s.empty() && (s.front() == ' ' || s.front() == '\t')) s.remove_prefix(1);
    while (!s.empty() && (s.back() == ' ' || s.back() == '\t')) s.remove_suffix(1);
    return s;
}
bool header_name(std::string_view s) {
    constexpr std::string_view punctuation = "!#$%&'*+-.^_`|~";
    return !s.empty() && std::all_of(s.begin(), s.end(), [&](unsigned char c) {
        return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') ||
            punctuation.find(static_cast<char>(c)) != std::string_view::npos;
    });
}
void admit_responses(const HttpHead& head, const Json& request, MimePolicy policy) {
    if (policy == MimePolicy::allow_missing_siwc_responses) {
        if (head.status == 401) fail(ErrorCode::transport, Stage::http, "auth_required");
        if (head.status == 403) fail(ErrorCode::transport, Stage::http, "provider_policy_denied");
        if (head.status == 429) fail(ErrorCode::transport, Stage::http, "provider_usage_limit");
        if (head.status != 200) fail(ErrorCode::transport, Stage::http, "provider_http_failure");
    }
    admit_http(head, responses_endpoint, "POST", request, policy);
}
struct Transfer {
    const Json& request;
    const std::function<void(std::string_view)>& sink;
    std::stop_token stop;
    MimePolicy policy;
    HttpHeadParser parser;
    std::size_t body = 0;
    bool admitted = false;
    std::optional<Error> error;
    bool callback_failed = false;
};
template<class T> void callback_error(T& transfer, const Error& error) noexcept {
    // Keep exceptions from crossing the C ABI, including allocation failures in diagnostics.
    try { if (!transfer.error) transfer.error = error; } catch (...) { transfer.callback_failed = true; }
}
std::size_t size_checked(std::size_t a, std::size_t b) {
    require(a == 0 || b <= std::numeric_limits<std::size_t>::max() / a, "curl_size_overflow", Stage::http);
    return a * b;
}
std::size_t receive_header(char* ptr, std::size_t size, std::size_t count, void* context) noexcept {
    auto& t = *static_cast<Transfer*>(context);
    try {
        require(!t.stop.stop_requested(), "http_cancelled", Stage::http, ErrorCode::cancelled);
        const auto n = size_checked(size, count);
        t.parser.line({ptr, n});
        return n;
    } catch (const Error& e) { callback_error(t, e); }
    catch (...) { t.callback_failed = true; }
    return CURL_WRITEFUNC_ERROR;
}
std::size_t receive_body(char* ptr, std::size_t size, std::size_t count, void* context) noexcept {
    auto& t = *static_cast<Transfer*>(context);
    try {
        require(!t.stop.stop_requested(), "http_cancelled", Stage::http, ErrorCode::cancelled);
        const auto n = size_checked(size, count);
        require(t.parser.complete(), "body_before_headers", Stage::http);
        if (!t.admitted) {
            admit_responses(t.parser.head(), t.request, t.policy); t.admitted = true;
        }
        require(n <= 16 * 1024 * 1024 - t.body, "http_body_limit", Stage::http, ErrorCode::limit);
        t.body += n;
        t.sink({ptr, n}); return n;
    } catch (const Error& e) { callback_error(t, e); }
    catch (...) { t.callback_failed = true; }
    return CURL_WRITEFUNC_ERROR;
}
int progress(void* ptr, curl_off_t, curl_off_t, curl_off_t, curl_off_t) noexcept {
    return static_cast<Transfer*>(ptr)->stop.stop_requested() ? 1 : 0;
}
template<class T> void option(CURL* curl, CURLoption key, T value) {
    require(curl_easy_setopt(curl, key, value) == CURLE_OK, "curl_option", Stage::http, ErrorCode::transport);
}
} // namespace
void HttpHeadParser::line(std::string_view line) {
    require(line.size() <= 64 * 1024 - bytes_, "http_header_limit", Stage::http, ErrorCode::limit);
    bytes_ += line.size();
    require(line.ends_with("\r\n"), "http_header_framing", Stage::http);
    line.remove_suffix(2);
    if (line.starts_with("HTTP/")) {
        require(!final_ && !complete_, "unexpected_http_status", Stage::http);
        const auto split = line.find(' ');
        require(split != std::string_view::npos && (line.substr(0, split) == "HTTP/1.1" ||
            line.substr(0, split) == "HTTP/1.0" || line.substr(0, split) == "HTTP/2" || line.substr(0, split) == "HTTP/3"),
            "http_version", Stage::http);
        const auto status = line.substr(split + 1, 3);
        long number = 0;
        const auto parsed = std::from_chars(status.data(), status.data() + status.size(), number);
        require(status.size() == 3 && parsed.ec == std::errc{} && parsed.ptr == status.data() + status.size() &&
                number >= 100 && number <= 599 && (line.size() == split + 4 || line[split + 4] == ' '), "http_status", Stage::http);
        require(head_.status == 0, "nested_http_status", Stage::http);
        head_ = {number, {}}; fields_ = 0;
    } else if (line.empty()) {
        require(head_.status != 0, "http_no_status", Stage::http);
        if (head_.status >= 100 && head_.status < 200) {
            require(head_.status != 101, "http_upgrade_forbidden", Stage::http);
            head_ = {}; // An interim block never admits a body.
        } else { complete_ = true; final_ = true; }
    } else {
        require(head_.status != 0 && ++fields_ <= 256, "http_header_count", Stage::http, ErrorCode::limit);
        const auto split = line.find(':');
        require(split != std::string_view::npos && header_name(line.substr(0, split)), "http_header_name", Stage::http);
        const auto name = lower(line.substr(0, split));
        const auto value = trim(line.substr(split + 1));
        require(std::none_of(value.begin(), value.end(), [](unsigned char c) { return (c < 0x20 && c != '\t') || c == 0x7f; }),
                "http_header_control", Stage::http);
        // MIME trailers cannot replace or supplement the head admitted before body delivery.
        if (complete_) require(name != "content-type", "http_mime_trailer", Stage::http, ErrorCode::mime);
        else if (name == "content-type") head_.content_types.emplace_back(value);
    }
}
void admit_http(const HttpHead& head, std::string_view endpoint, std::string_view method, const Json& request, MimePolicy policy) {
    require(endpoint == responses_endpoint && method == "POST", "http_endpoint", Stage::http, ErrorCode::transport);
    require(request.at("stream") == Json::parse("true") && request.at("store") == Json::parse("false"), "http_request_profile", Stage::http);
    require(head.status == 200, "http_unsuccessful", Stage::http, ErrorCode::transport);
    require(head.content_types.size() <= 1, "http_duplicate_mime", Stage::http, ErrorCode::mime);
    if (head.content_types.empty()) {
        require(policy == MimePolicy::allow_missing_siwc_responses, "http_missing_mime", Stage::http, ErrorCode::mime);
        return;
    }
    const auto value = trim(head.content_types[0]);
    const auto semicolon = value.find(';');
    require(lower(trim(value.substr(0, semicolon))) == "text/event-stream", "http_wrong_mime", Stage::http, ErrorCode::mime);
    if (semicolon != std::string_view::npos) {
        // One narrow parameter grammar; reject comma-joined/contradictory MIME values.
        const auto parameter = lower(trim(value.substr(semicolon + 1)));
        require(parameter == "charset=utf-8" || parameter == "charset=\"utf-8\"", "http_mime_parameter", Stage::http, ErrorCode::mime);
    }
}
DependencyInfo dependency_smoke() {
    initialize();
    Easy easy(curl_easy_init(), curl_easy_cleanup);
    require(bool(easy), "curl_easy_init", Stage::http);
    option(easy.get(), CURLOPT_SSL_VERIFYPEER, 1L); option(easy.get(), CURLOPT_SSL_VERIFYHOST, 2L);
    std::unique_ptr<CURLU, decltype(&curl_url_cleanup)> url(curl_url(), curl_url_cleanup);
    require(url && curl_url_set(url.get(), CURLUPART_URL, responses_endpoint.data(), 0) == CURLUE_OK, "curl_url", Stage::http);
    const auto* curl = curl_version_info(CURLVERSION_NOW);
    require(curl && (curl->features & CURL_VERSION_SSL) && curl->ssl_version, "curl_tls_unavailable", Stage::http);
    require(Json::parse(R"({"linked":true})").has("linked") &&
        sha256("abc") == "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", "dependency_smoke", Stage::json);
#if defined(__aarch64__)
    const std::string target = "aarch64-linux";
#elif defined(__x86_64__)
    const std::string target = "x86_64-linux";
#else
    const std::string target = "other";
#endif
    return {curl->version, curl->ssl_version, YYJSON_VERSION_STRING, OpenSSL_version(OPENSSL_VERSION), target};
}
Result<std::monostate> CurlTransport::post(const Json& request, std::string_view bearer,
    const std::function<void(std::string_view)>& sink, std::stop_token stop, MimePolicy policy) const {
    return capture([&] {
        require(!stop.stop_requested(), "http_cancelled", Stage::http, ErrorCode::cancelled);
        require(!bearer.empty() && bearer.size() <= 16384 && std::all_of(bearer.begin(), bearer.end(), [](unsigned char c) {
            return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') ||
                c == '-' || c == '_' || c == '.' || c == '~' || c == '+' || c == '/' || c == '=';
        }), "bearer_shape", Stage::http, ErrorCode::validation);
        initialize();
        Easy curl(curl_easy_init(), curl_easy_cleanup); require(bool(curl), "curl_easy_init", Stage::http);
        auto payload = request.dump();
        Transfer transfer{request, sink, stop, policy, {}, 0, false, {}};
        Headers headers(nullptr, curl_slist_free_all);
        for (const auto& header : {std::string("Content-Type: application/json"), std::string("Accept: text/event-stream"),
                                  std::string("Authorization: Bearer ") + std::string(bearer)}) {
            auto* appended = curl_slist_append(headers.get(), header.c_str());
            require(appended != nullptr, "curl_header_allocation", Stage::http);
            (void)headers.release(); headers.reset(appended);
        }
        option(curl.get(), CURLOPT_URL, responses_endpoint.data());
        option(curl.get(), CURLOPT_PROTOCOLS_STR, "https"); option(curl.get(), CURLOPT_REDIR_PROTOCOLS_STR, "https");
        option(curl.get(), CURLOPT_FOLLOWLOCATION, 0L); option(curl.get(), CURLOPT_MAXREDIRS, 0L);
        option(curl.get(), CURLOPT_SSL_VERIFYPEER, 1L); option(curl.get(), CURLOPT_SSL_VERIFYHOST, 2L);
        option(curl.get(), CURLOPT_NOSIGNAL, 1L); option(curl.get(), CURLOPT_CONNECTTIMEOUT_MS, 6000L);
        option(curl.get(), CURLOPT_TIMEOUT_MS, 75000L); option(curl.get(), CURLOPT_NOPROGRESS, 0L);
        option(curl.get(), CURLOPT_XFERINFOFUNCTION, &progress); option(curl.get(), CURLOPT_XFERINFODATA, &transfer);
        option(curl.get(), CURLOPT_HEADERFUNCTION, &receive_header); option(curl.get(), CURLOPT_HEADERDATA, &transfer);
        option(curl.get(), CURLOPT_WRITEFUNCTION, &receive_body); option(curl.get(), CURLOPT_WRITEDATA, &transfer);
        option(curl.get(), CURLOPT_HTTPHEADER, headers.get()); option(curl.get(), CURLOPT_POST, 1L);
        option(curl.get(), CURLOPT_SUPPRESS_CONNECT_HEADERS, 1L);
        option(curl.get(), CURLOPT_POSTFIELDS, payload.data());
        option(curl.get(), CURLOPT_POSTFIELDSIZE_LARGE, static_cast<curl_off_t>(payload.size()));
        const auto status = curl_easy_perform(curl.get());
        if (transfer.error) throw *transfer.error;
        require(!transfer.callback_failed, "http_callback_failure", Stage::http, ErrorCode::transport);
        require(!stop.stop_requested(), "http_cancelled", Stage::http, ErrorCode::cancelled);
        require(status == CURLE_OK, "http_transfer_failed", Stage::http, ErrorCode::transport);
        require(transfer.parser.complete(), "http_missing_headers", Stage::http);
        admit_responses(transfer.parser.head(), request, policy);
        return std::monostate{};
    });
}
Result<CompletedResponse> SiwcProvider::generate(const Request& request, std::string_view bearer,
    std::stop_token stop, MimePolicy policy, TextPreview preview) const {
    return capture([&]() -> CompletedResponse {
        ResponseStream stream(request, {}, stop, std::move(preview));
        const auto status = CurlTransport{}.post(request.json(), bearer,
            [&](std::string_view bytes) { stream.feed(bytes); }, stop, policy);
        if (const auto* error = std::get_if<Error>(&status)) throw *error;
        auto response = stream.finish();
        if (const auto* error = std::get_if<Error>(&response)) throw *error;
        return std::get<CompletedResponse>(std::move(response));
    });
}
Result<CompletedResponse> decode_offline(const Request& request, const HttpHead& head,
    const std::vector<std::string>& chunks, Limits limits, MimePolicy policy, std::stop_token stop, TextPreview preview) {
    return capture([&]() -> CompletedResponse {
        admit_http(head, responses_endpoint, "POST", request.json(), policy);
        ResponseStream stream(request, limits, stop, std::move(preview));
        for (const auto& chunk : chunks) stream.feed(chunk);
        auto response = stream.finish();
        if (const auto* error = std::get_if<Error>(&response)) throw *error;
        return std::get<CompletedResponse>(std::move(response));
    });
}
namespace {
struct BufferedTransfer {
    HttpHeadParser parser;
    std::string body;
    std::size_t limit;
    std::stop_token stop;
    std::optional<Error> error;
    bool callback_failed = false;
};
std::size_t buffered_head(char* bytes, std::size_t size, std::size_t count, void* opaque) noexcept {
    auto& transfer = *static_cast<BufferedTransfer*>(opaque);
    try {
        const auto length = size_checked(size, count);
        transfer.parser.line({bytes, length});
        return length;
    } catch (const Error& error) { callback_error(transfer, error); }
    catch (...) { transfer.callback_failed = true; }
    return CURL_WRITEFUNC_ERROR;
}
std::size_t buffered_body(char* bytes, std::size_t size, std::size_t count, void* opaque) noexcept {
    auto& transfer = *static_cast<BufferedTransfer*>(opaque);
    try {
        const auto length = size_checked(size, count);
        require(transfer.parser.complete() && length <= transfer.limit - transfer.body.size(),
                "http_body_limit", Stage::http, ErrorCode::limit);
        transfer.body.append(bytes, length);
        return length;
    } catch (const Error& error) { callback_error(transfer, error); }
    catch (...) { transfer.callback_failed = true; }
    return CURL_WRITEFUNC_ERROR;
}
int buffered_progress(void* opaque, curl_off_t, curl_off_t, curl_off_t, curl_off_t) noexcept {
    return static_cast<BufferedTransfer*>(opaque)->stop.stop_requested() ? 1 : 0;
}
} // namespace

HttpReply http_exchange(const HttpRequest& request, std::stop_token stop) {
    require(!stop.stop_requested(), "http_cancelled", Stage::http, ErrorCode::cancelled);
    require((request.url.starts_with("https://auth.openai.com/") ||
             request.url == responses_endpoint || request.url == "https://api.openai.com/v1/models") &&
            request.url.find_first_of("\r\n?#@") == std::string::npos &&
            (request.method == "GET" || request.method == "POST") &&
            request.body.size() <= 16 * 1024 * 1024 && request.body_limit <= 16 * 1024 * 1024,
            "http_request_contract", Stage::http);
    require(request.bearer.find_first_of("\r\n") == std::string::npos &&
            request.bearer.size() <= 16384 &&
            (request.content_type.empty() || request.content_type == "application/json" ||
             request.content_type == "application/x-www-form-urlencoded"), "http_header_contract", Stage::http);
    initialize();
    Easy curl(curl_easy_init(), curl_easy_cleanup);
    require(bool(curl), "curl_easy_init", Stage::http);
    BufferedTransfer transfer{{}, {}, request.body_limit, stop, {}};
    Headers headers(nullptr, curl_slist_free_all);
    std::vector<std::string> fields{"Accept: application/json, text/event-stream"};
    if (!request.content_type.empty()) fields.push_back("Content-Type: " + request.content_type);
    if (!request.bearer.empty()) fields.push_back("Authorization: Bearer " + request.bearer);
    for (const auto& field : fields) {
        auto* added = curl_slist_append(headers.get(), field.c_str());
        require(added != nullptr, "curl_header_allocation", Stage::http);
        (void)headers.release();
        headers.reset(added);
    }
    option(curl.get(), CURLOPT_URL, request.url.c_str());
    option(curl.get(), CURLOPT_PROTOCOLS_STR, "https");
    option(curl.get(), CURLOPT_FOLLOWLOCATION, 0L);
    option(curl.get(), CURLOPT_SSL_VERIFYPEER, 1L);
    option(curl.get(), CURLOPT_SSL_VERIFYHOST, 2L);
    option(curl.get(), CURLOPT_NOSIGNAL, 1L);
    option(curl.get(), CURLOPT_CONNECTTIMEOUT_MS, 6000L);
    option(curl.get(), CURLOPT_TIMEOUT_MS, request.timeout_ms);
    option(curl.get(), CURLOPT_USERAGENT, "ansa/0.1");
    option(curl.get(), CURLOPT_NOPROGRESS, 0L);
    option(curl.get(), CURLOPT_XFERINFOFUNCTION, &buffered_progress);
    option(curl.get(), CURLOPT_XFERINFODATA, &transfer);
    option(curl.get(), CURLOPT_HEADERFUNCTION, &buffered_head);
    option(curl.get(), CURLOPT_HEADERDATA, &transfer);
    option(curl.get(), CURLOPT_WRITEFUNCTION, &buffered_body);
    option(curl.get(), CURLOPT_WRITEDATA, &transfer);
    option(curl.get(), CURLOPT_HTTPHEADER, headers.get());
    option(curl.get(), CURLOPT_SUPPRESS_CONNECT_HEADERS, 1L);
    if (request.method == "POST") {
        option(curl.get(), CURLOPT_POST, 1L);
        option(curl.get(), CURLOPT_POSTFIELDS, request.body.data());
        option(curl.get(), CURLOPT_POSTFIELDSIZE_LARGE, static_cast<curl_off_t>(request.body.size()));
    }
    const auto status = curl_easy_perform(curl.get());
    if (transfer.error) throw *transfer.error;
    require(!transfer.callback_failed, "http_callback_failure", Stage::http, ErrorCode::transport);
    require(!stop.stop_requested(), "http_cancelled", Stage::http, ErrorCode::cancelled);
    if (status == CURLE_OPERATION_TIMEDOUT) fail(ErrorCode::transport, Stage::http, "http_timeout");
    if (status == CURLE_COULDNT_RESOLVE_HOST) fail(ErrorCode::transport, Stage::http, "http_resolve_failed");
    if (status == CURLE_COULDNT_CONNECT) fail(ErrorCode::transport, Stage::http, "http_connect_failed");
    if (status == CURLE_PEER_FAILED_VERIFICATION) fail(ErrorCode::transport, Stage::http, "http_tls_verification");
    require(status == CURLE_OK, "http_transfer_failed", Stage::http, ErrorCode::transport);
    require(transfer.parser.complete(), "http_missing_headers", Stage::http, ErrorCode::transport);
    return {transfer.parser.head(), std::move(transfer.body)};
}
} // namespace omh
