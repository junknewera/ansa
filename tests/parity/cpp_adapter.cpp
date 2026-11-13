#include "omh/session.hpp"
#include "omh/transport.hpp"
#include <iostream>
#include <curl/curl.h>
#include <algorithm>
#include <charconv>
#include <limits>

using namespace omh;
namespace {
struct Wire {
    HttpHeadParser parser;
    std::string body;
    std::size_t limit;
    bool failed = false;
    std::vector<Json> interim, trailers;
};
std::string lower(std::string_view text) {
    std::string value(text);
    for (auto& c : value) if (c >= 'A' && c <= 'Z') c = static_cast<char>(c + 'a' - 'A');
    return value;
}
std::size_t head_callback(char* bytes, std::size_t size, std::size_t count, void* opaque) noexcept {
    auto& wire = *static_cast<Wire*>(opaque);
    try {
        require(size == 0 || count <= std::numeric_limits<std::size_t>::max() / size, "risk_header_overflow");
        const std::string_view line(bytes, size * count);
        const bool trailer = wire.parser.complete();
        wire.parser.line(line); // Production parser, including every trailer.
        if (line.starts_with("HTTP/")) {
            const auto split = line.find(' ');
            unsigned status = 0;
            std::from_chars(line.data() + split + 1, line.data() + split + 4, status);
            if (status < 200) wire.interim.push_back(Json::parse(std::to_string(status)));
        } else if (trailer && line != "\r\n") {
            const auto colon = line.find(':');
            auto value = line.substr(colon + 1, line.size() - colon - 3);
            while (!value.empty() && (value.front() == ' ' || value.front() == '\t')) value.remove_prefix(1);
            while (!value.empty() && (value.back() == ' ' || value.back() == '\t')) value.remove_suffix(1);
            wire.trailers.push_back(Json::string(lower(line.substr(0, colon)) + ":" + std::string(value)));
        }
        return line.size();
    } catch (...) { wire.failed = true; return 0; }
}
std::size_t body_callback(char* bytes, std::size_t size, std::size_t count, void* opaque) noexcept {
    auto& wire = *static_cast<Wire*>(opaque);
    try {
        require(size == 0 || count <= std::numeric_limits<std::size_t>::max() / size, "risk_body_overflow");
        const auto length = size * count;
        require(wire.parser.complete() && length <= wire.limit - wire.body.size(), "risk_body_limit");
        admit_http(wire.parser.head(), responses_endpoint, "POST", Json::parse(R"({"stream":true,"store":false})"), MimePolicy::strict);
        wire.body.append(bytes, length);
        return length;
    } catch (...) { wire.failed = true; return 0; }
}
Json http_probe(const Json& request) {
    std::size_t received = 0;
    const auto rejected = [&] { return Json::object({{"accepted", Json::parse("false")}, {"executors", Json::parse("0")},
        {"uncommitted_body_bytes", Json::parse(std::to_string(received))}}); };
    const auto url = request.text("url");
    std::unique_ptr<CURLU, decltype(&curl_url_cleanup)> parsed(curl_url(), curl_url_cleanup);
    if (!parsed || curl_url_set(parsed.get(), CURLUPART_URL, url.c_str(), 0) != CURLUE_OK) return rejected();
    char *host_raw = nullptr, *scheme_raw = nullptr;
    if (curl_url_get(parsed.get(), CURLUPART_HOST, &host_raw, 0) != CURLUE_OK) return rejected();
    std::unique_ptr<char, decltype(&curl_free)> host(host_raw, curl_free);
    if (curl_url_get(parsed.get(), CURLUPART_SCHEME, &scheme_raw, 0) != CURLUE_OK) return rejected();
    std::unique_ptr<char, decltype(&curl_free)> scheme(scheme_raw, curl_free);
    if ((std::string_view(host.get()) != "localhost" && std::string_view(host.get()) != "127.0.0.1" && std::string_view(host.get()) != "[::1]") ||
        (std::string_view(scheme.get()) != "http" && std::string_view(scheme.get()) != "https")) return rejected();
    const auto limit = request.has("body_limit") ? request.index("body_limit") : 16 * 1024 * 1024;
    if (!limit || limit > 16 * 1024 * 1024) return rejected();
    std::unique_ptr<CURL, decltype(&curl_easy_cleanup)> curl(curl_easy_init(), curl_easy_cleanup);
    if (!curl) return rejected();
    Wire wire{{}, {}, limit};
    curl_easy_setopt(curl.get(), CURLOPT_URL, url.c_str());
    curl_easy_setopt(curl.get(), CURLOPT_PROTOCOLS_STR, "http,https");
    curl_easy_setopt(curl.get(), CURLOPT_FOLLOWLOCATION, 0L);
    curl_easy_setopt(curl.get(), CURLOPT_SSL_VERIFYPEER, 1L);
    curl_easy_setopt(curl.get(), CURLOPT_SSL_VERIFYHOST, 2L);
    curl_easy_setopt(curl.get(), CURLOPT_PROXY_SSL_VERIFYPEER, 1L);
    curl_easy_setopt(curl.get(), CURLOPT_PROXY_SSL_VERIFYHOST, 2L);
    curl_easy_setopt(curl.get(), CURLOPT_IPRESOLVE, CURL_IPRESOLVE_V4);
    curl_easy_setopt(curl.get(), CURLOPT_CONNECTTIMEOUT_MS, 1000L);
    curl_easy_setopt(curl.get(), CURLOPT_TIMEOUT_MS, 3000L);
    curl_easy_setopt(curl.get(), CURLOPT_NOSIGNAL, 1L);
    curl_easy_setopt(curl.get(), CURLOPT_POSTFIELDS, "{\"stream\":true,\"store\":false}");
    curl_easy_setopt(curl.get(), CURLOPT_SUPPRESS_CONNECT_HEADERS, 1L);
    curl_easy_setopt(curl.get(), CURLOPT_HEADERFUNCTION, head_callback);
    curl_easy_setopt(curl.get(), CURLOPT_HEADERDATA, &wire);
    curl_easy_setopt(curl.get(), CURLOPT_WRITEFUNCTION, body_callback);
    curl_easy_setopt(curl.get(), CURLOPT_WRITEDATA, &wire);
    std::string ca;
    if (request.has("ca_pem")) {
        ca = request.text("ca_pem");
        curl_easy_setopt(curl.get(), CURLOPT_CAINFO, ca.c_str());
        curl_easy_setopt(curl.get(), CURLOPT_PROXY_CAINFO, ca.c_str());
    }
    if (request.has("h2") && request.at("h2") == Json::parse("true")) curl_easy_setopt(curl.get(), CURLOPT_HTTP_VERSION, CURL_HTTP_VERSION_2_PRIOR_KNOWLEDGE);
    const auto status = curl_easy_perform(curl.get());
    received = wire.body.size();
    if (status != CURLE_OK || wire.failed || !wire.parser.complete()) return rejected();
    try { admit_http(wire.parser.head(), responses_endpoint, "POST", Json::parse(R"({"stream":true,"store":false})"), MimePolicy::strict); }
    catch (...) { return rejected(); }
    std::string hex;
    constexpr char digits[] = "0123456789abcdef";
    for (unsigned char c : wire.body) { hex += digits[c >> 4]; hex += digits[c & 15]; }
    std::sort(wire.trailers.begin(), wire.trailers.end(), [](const Json& a, const Json& b) { return a.text() < b.text(); });
    return Json::object({{"accepted", Json::parse("true")}, {"executors", Json::parse("0")}, {"body_hex", Json::string(hex)},
        {"interim", Json::array(wire.interim)}, {"trailers", Json::array(wire.trailers)}});
}
Json domains(const Json& input) {
    ToolRegistry registry;
    std::vector<Json> records;
    for (const auto& value : input.at("tools").elements()) {
        auto descriptor = Json::object({{"name", value.at("name")}, {"description", value.at("description")},
            {"implementation_revision", value.at("implementation_revision")}, {"schema", value.at("schema")}});
        const bool journal = value.at("requires_journal") == Json::parse("true");
        registry.register_tool({{value.text("name"), value.text("description"), value.text("implementation_revision"), value.at("schema")},
            [](const Json&) { return true; }, [](const Json&, std::stop_token) { return ToolOutcome{Execution::succeeded, Effects::known_result, ""}; }, {}, {}, journal, true});
        records.push_back(journal ? Json::object({{"descriptor", descriptor}, {"requires_journal", Json::parse("true")}}) : descriptor);
    }
    const auto tools = registry.snapshot()->advertisement();
    const auto epoch = input.at("epoch");
    ContextEpoch base{epoch.text("id"), epoch.text("instructions"), epoch.at("state")};
    Request config(Binding{}, base, tools, {});
    SessionState state({"risk"}, config, tools);
    const auto plan = state.request_for("fixture", AgentMode::plan).epoch();
    auto native = input.at("compact");
    auto admitted = CompactedContext::admit(config, native);
    if (const auto* error = std::get_if<Error>(&admitted)) throw *error;
    const auto compact = compact_epoch(base, 1, std::get<CompactedContext>(admitted));
    std::vector<Json> files;
    for (const auto& file : input.at("instructions").elements())
        files.push_back(Json::object({{"path", file.at("path")}, {"text", file.at("text")}}));
    const auto instructions = Json::array(files);
    const auto invocation = input.at("invocation");
    auto saved = save_invocation({invocation.text("operation"), invocation.text("call"), invocation.text("name"),
        invocation.text("revision"), invocation.at("arguments")});
    auto evidence = Json::object({{"version", Json::parse("1")}, {"invocation", saved},
        {"cwd", input.at("cwd")}, {"argv", input.at("argv")},
        {"timeout_ms", input.at("timeout_ms")}, {"output_bytes", input.at("output_bytes")}});
    return Json::object({{"tool", Json::object({{"bytes", Json::string(Json::array(records).dump())}, {"hash", Json::string(tools.revision)}})},
        {"instruction", Json::object({{"bytes", Json::string(instructions.dump())}, {"hash", Json::string(sha256(instructions.dump()))}})},
        {"plan", Json::object({{"bytes", Json::string(plan.exact_state.dump())}, {"hash", Json::string(plan.id)}})},
        {"compact", Json::object({{"bytes", Json::string(native.dump())}, {"hash", Json::string(compact.id)}})},
        {"evidence", Json::object({{"bytes", Json::string(evidence.dump())}, {"hash", Json::string(sha256(evidence.dump()))}})}});
}
Json evaluate(const Json& request) {
    const auto op = request.text("op");
    if (op == "http") return http_probe(request);
    if (op == "json") {
        try {
            const auto value = Json::parse(request.text("payload"));
            std::vector<std::pair<std::string, Json>> result{{"accepted", Json::parse("true")}, {"dump", Json::string(value.dump())},
                {"sha256", Json::string(sha256(value.dump()))}};
            if (request.has("other")) result.emplace_back("equal", Json::parse(value == Json::parse(request.text("other")) ? "true" : "false"));
            return Json::object(result);
        } catch (const Error&) { return Json::object({{"accepted", Json::parse("false")}}); }
    }
    if (op == "domains") return domains(Json::parse(request.text("payload")));
    if (op == "headers") {
        try {
            HttpHeadParser parser;
            for (const auto& line : request.at("lines").elements()) parser.line(line.text());
            require(parser.complete(), "risk_incomplete_head");
            admit_http(parser.head(), responses_endpoint, "POST", Json::parse(R"({"stream":true,"store":false})"), MimePolicy::strict);
            return Json::object({{"accepted", Json::parse("true")}});
        } catch (const Error&) { return Json::object({{"accepted", Json::parse("false")}}); }
    }
    fail(ErrorCode::contract, Stage::frontend, "risk_unknown_operation");
}
}
int main() {
    if (curl_global_init(CURL_GLOBAL_DEFAULT) != CURLE_OK) return 2;
    struct Cleanup { ~Cleanup() { curl_global_cleanup(); } } cleanup;
    std::string line;
    const auto emit = [&]() {
        try { std::cout << evaluate(Json::parse(line)).dump() << '\n'; }
        catch (...) { std::cerr << "risk_adapter_failure\n"; return false; }
        return true;
    };
    char byte;
    while (std::cin.get(byte)) {
        if (byte == '\n') { if (!emit()) return 1; line.clear(); }
        else { if (line.size() >= 2 * 1024 * 1024) return 2; line += byte; }
    }
    if (!line.empty() && !emit()) return 1;
    return std::cin.bad() ? 1 : 0;
}
