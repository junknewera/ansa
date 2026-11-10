#include "omh/json.hpp"
#include <openssl/evp.h>
#include <cmath>
#include <cstdlib>
#include <limits>
#include <set>

namespace omh {
namespace {
yyjson_val* mutable_ptr(const yyjson_val* v) { return const_cast<yyjson_val*>(v); }
std::string write(const yyjson_val* v) {
    std::size_t n = 0;
    std::unique_ptr<char, decltype(&std::free)> out(yyjson_val_write(v, 0, &n), &std::free);
    require(bool(out), "json_write", Stage::json);
    return {out.get(), n};
}
void inspect(yyjson_val* v, std::size_t depth, std::size_t& nodes, const std::size_t max_nodes) {
    require(depth > 0 && ++nodes <= max_nodes, "json_structure_limit", Stage::json, ErrorCode::limit);
    if (yyjson_is_real(v)) require(std::isfinite(yyjson_get_real(v)), "json_nonfinite", Stage::json);
    if (yyjson_is_obj(v)) {
        std::set<std::string> keys;
        yyjson_obj_iter it = yyjson_obj_iter_with(v);
        while (auto* key = yyjson_obj_iter_next(&it)) {
            std::string k(yyjson_get_str(key), yyjson_get_len(key));
            require(keys.insert(k).second, "json_duplicate_key", Stage::json);
            inspect(yyjson_obj_iter_get_val(key), depth - 1, nodes, max_nodes);
        }
    } else if (yyjson_is_arr(v)) {
        yyjson_arr_iter it = yyjson_arr_iter_with(v);
        while (auto* child = yyjson_arr_iter_next(&it)) inspect(child, depth - 1, nodes, max_nodes);
    }
}
bool equal(yyjson_val* a, yyjson_val* b) {
    if (yyjson_get_type(a) != yyjson_get_type(b)) return false;
    if (yyjson_is_obj(a)) {
        if (yyjson_obj_size(a) != yyjson_obj_size(b)) return false;
        yyjson_obj_iter it = yyjson_obj_iter_with(a);
        while (auto* key = yyjson_obj_iter_next(&it)) {
            auto* other = yyjson_obj_getn(b, yyjson_get_str(key), yyjson_get_len(key));
            if (!other || !equal(yyjson_obj_iter_get_val(key), other)) return false;
        }
        return true;
    }
    if (yyjson_is_arr(a)) {
        if (yyjson_arr_size(a) != yyjson_arr_size(b)) return false;
        for (std::size_t i = 0; i < yyjson_arr_size(a); ++i)
            if (!equal(yyjson_arr_get(a, i), yyjson_arr_get(b, i))) return false;
        return true;
    }
    return yyjson_equals(a, b);
}
void append(std::string& out, std::string_view part) {
    require(part.size() <= 16 * 1024 * 1024 - out.size(), "json_build_limit", Stage::json, ErrorCode::limit);
    out.append(part);
}
} // namespace
Json::Json(yyjson_doc* doc) : doc_(doc, yyjson_doc_free) {}
Json Json::parse(std::string_view bytes, std::size_t max_bytes, std::size_t depth, std::size_t max_nodes) {
    require(!bytes.empty(), "json_empty", Stage::json);
    require(bytes.size() <= max_bytes, "json_byte_limit", Stage::json, ErrorCode::limit);
    // Bound nesting before yyjson allocates its parsing stack.
    bool quoted = false, escape = false;
    std::size_t level = 0;
    for (const char c : bytes) {
        if (quoted) {
            if (escape) escape = false;
            else if (c == '\\') escape = true;
            else if (c == '"') quoted = false;
        } else if (c == '"') quoted = true;
        else if (c == '[' || c == '{') {
            require(++level < depth, "json_depth_limit", Stage::json, ErrorCode::limit);
        } else if (c == ']' || c == '}') { if (level) --level; }
    }
    // Strict RFC 8259: no comments, invalid Unicode, trailing data, NaN or Infinity.
    auto* doc = yyjson_read(bytes.data(), bytes.size(), 0);
    require(doc != nullptr, "json_invalid", Stage::json);
    Json result(doc);
    std::size_t nodes = 0;
    inspect(yyjson_doc_get_root(doc), depth, nodes, max_nodes);
    return result;
}
Json Json::string(std::string_view text) {
    std::unique_ptr<yyjson_mut_doc, decltype(&yyjson_mut_doc_free)> doc(yyjson_mut_doc_new(nullptr), yyjson_mut_doc_free);
    require(bool(doc), "json_allocation", Stage::json);
    auto* str = yyjson_mut_strncpy(doc.get(), text.data(), text.size());
    require(str != nullptr, "json_allocation", Stage::json);
    yyjson_mut_doc_set_root(doc.get(), str);
    std::size_t len = 0;
    std::unique_ptr<char, decltype(&std::free)> bytes(yyjson_mut_write(doc.get(), 0, &len), &std::free);
    require(bool(bytes), "json_string_invalid", Stage::json);
    return parse(std::string_view(bytes.get(), len));
}
Json Json::object(const std::vector<std::pair<std::string, Json>>& entries) {
    std::string out = "{";
    for (std::size_t i = 0; i < entries.size(); ++i) {
        if (i) append(out, ",");
        append(out, string(entries[i].first).dump()); append(out, ":"); append(out, entries[i].second.dump());
    }
    append(out, "}"); return parse(out);
}
Json Json::array(const std::vector<Json>& entries) {
    std::string out = "[";
    for (std::size_t i = 0; i < entries.size(); ++i) {
        if (i) append(out, ",");
        append(out, entries[i].dump());
    }
    append(out, "]"); return parse(out);
}
yyjson_val* Json::raw() const { return yyjson_doc_get_root(doc_.get()); }
std::string Json::dump() const { return write(raw()); }
bool Json::is_object() const { return yyjson_is_obj(raw()); }
bool Json::is_array() const { return yyjson_is_arr(raw()); }
bool Json::is_null() const { return yyjson_is_null(raw()); }
bool Json::is_string() const { return yyjson_is_str(raw()); }
bool Json::is_integer() const { return yyjson_is_int(raw()); }
bool Json::is_number() const { return yyjson_is_num(raw()); }
bool Json::is_boolean() const { return yyjson_is_bool(raw()); }
std::vector<std::string> Json::keys() const {
    require(is_object(), "json_expected_object", Stage::json);
    std::vector<std::string> out;
    yyjson_obj_iter it = yyjson_obj_iter_with(raw());
    while (auto* key = yyjson_obj_iter_next(&it)) out.emplace_back(yyjson_get_str(key), yyjson_get_len(key));
    return out;
}
bool Json::has(const char* key) const { return yyjson_obj_get(mutable_ptr(raw()), key) != nullptr; }
Json Json::at(const char* key) const {
    auto* v = yyjson_obj_get(mutable_ptr(raw()), key);
    require(v != nullptr, "json_missing_field", Stage::json);
    return parse(write(v));
}
std::string Json::text() const {
    require(yyjson_is_str(raw()), "json_expected_string", Stage::json);
    return {yyjson_get_str(raw()), yyjson_get_len(raw())};
}
std::string Json::text(const char* key) const { return at(key).text(); }
std::size_t Json::index(const char* key) const {
    const auto v = at(key);
    require(yyjson_is_uint(v.raw()), "json_expected_unsigned_integer", Stage::json);
    const auto n = yyjson_get_uint(v.raw());
    require(n <= std::numeric_limits<std::size_t>::max(), "json_integer_overflow", Stage::json);
    return static_cast<std::size_t>(n);
}
std::vector<Json> Json::elements() const {
    require(is_array(), "json_expected_array", Stage::json);
    std::vector<Json> out;
    yyjson_arr_iter it = yyjson_arr_iter_with(mutable_ptr(raw()));
    while (auto* v = yyjson_arr_iter_next(&it)) out.push_back(parse(write(v)));
    return out;
}
bool Json::operator==(const Json& other) const { return equal(mutable_ptr(raw()), mutable_ptr(other.raw())); }
std::string sha256(std::string_view bytes) {
    unsigned char digest[EVP_MAX_MD_SIZE];
    unsigned len = 0;
    require(EVP_Digest(bytes.data(), bytes.size(), digest, &len, EVP_sha256(), nullptr) == 1 && len == 32,
            "crypto_digest", Stage::json);
    constexpr char hex[] = "0123456789abcdef";
    std::string out;
    for (unsigned i = 0; i < len; ++i) { out += hex[digest[i] >> 4]; out += hex[digest[i] & 15]; }
    return out;
}
Json diagnostic_json(const Error& e) {
    return Json::object({{"schema_version", Json::parse("1")},
        {"code", Json::parse(std::to_string(static_cast<unsigned>(e.code)))},
        {"stage", Json::parse(std::to_string(static_cast<unsigned>(e.stage)))},
        {"diagnostic", Json::string(e.diagnostic)}});
}
} // namespace omh
