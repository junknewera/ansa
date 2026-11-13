#pragma once
#include <yyjson.h>
#include <cstddef>
#include <cstdint>
#include <exception>
#include <memory>
#include <string>
#include <string_view>
#include <utility>
#include <variant>
#include <vector>

namespace omh {
enum class ErrorCode { protocol, limit, transport, mime, validation, cancelled, tool, contract,
                       response_failed, response_incomplete, premature_eof };
enum class Stage { json, framing, admission, request, preparation, execution, http, frontend };
struct Error final : std::exception {
    ErrorCode code;
    Stage stage;
    std::string diagnostic; // Fixed local code only; never peer payload or credentials.
    Error(ErrorCode c, Stage s, std::string d) : code(c), stage(s), diagnostic(std::move(d)) {}
    const char* what() const noexcept override { return diagnostic.c_str(); }
};
[[noreturn]] inline void fail(ErrorCode c, Stage s, const char* d) { throw Error(c, s, d); }
inline void require(bool ok, const char* d, Stage s = Stage::admission,
                    ErrorCode c = ErrorCode::protocol) { if (!ok) fail(c, s, d); }
template<class T> using Result = std::variant<T, Error>;
template<class F> auto capture(F&& f) -> Result<decltype(f())> {
    try { return f(); }
    catch (const Error& e) { return e; }
    catch (...) { return Error(ErrorCode::contract, Stage::admission, "internal_failure"); }
}
struct Limits {
    std::size_t line = 1024 * 1024;
    std::size_t event = 1024 * 1024;
    std::size_t body = 16 * 1024 * 1024;
    std::size_t arguments = 256 * 1024;
    std::size_t items = 64;
    std::size_t calls = 16;
    std::size_t events = 65536;
    std::size_t json_depth = 64;
    std::size_t json_nodes = 131072;
};

// Immutable, owning document. No borrowed parser values cross a public boundary.
class Json {
    std::shared_ptr<yyjson_doc> doc_;
    std::size_t max_bytes_, max_depth_, max_nodes_;
    explicit Json(yyjson_doc* doc, std::size_t max_bytes = 16 * 1024 * 1024,
                  std::size_t max_depth = 64, std::size_t max_nodes = 131072);
    yyjson_val* raw() const;
public:
    static Json parse(std::string_view bytes, std::size_t max_bytes = 16 * 1024 * 1024,
                      std::size_t depth = 64, std::size_t nodes = 131072);
    static Json string(std::string_view, std::size_t max_bytes = 16 * 1024 * 1024);
    static Json object(const std::vector<std::pair<std::string, Json>>&,
                       std::size_t max_bytes = 16 * 1024 * 1024);
    static Json array(const std::vector<Json>&);
    std::string dump() const;
    bool is_object() const;
    bool is_array() const;
    bool is_null() const;
    bool is_string() const;
    bool is_integer() const;
    bool is_number() const;
    bool is_boolean() const;
    std::vector<std::string> keys() const;
    bool has(std::string_view key) const;
    Json at(std::string_view key) const;
    std::string text() const;
    std::string text(const char* key) const;
    std::size_t index(const char* key) const;
    std::vector<Json> elements() const;
    bool operator==(const Json&) const;
};
std::string sha256(std::string_view);
Json diagnostic_json(const Error&);
} // namespace omh
