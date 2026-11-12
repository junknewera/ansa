#include "omh/local_tools.hpp"
#include "omh/local_storage.hpp"
#include <algorithm>
#include <cerrno>
#include <fcntl.h>
#include <sys/stat.h>
#include <unistd.h>

namespace omh {
namespace {
Json number(std::size_t value) { return Json::parse(std::to_string(value)); }
Json boolean(bool value) { return Json::parse(value ? "true" : "false"); }
ToolOutcome result(Json value) {
    auto text = value.dump();
    if (text.size() > 120 * 1024) text = R"({"error":"tool_output_limit"})";
    return {Execution::succeeded, Effects::known_result, std::move(text)};
}
ToolOutcome error(const char* code) { return result(Json::object({{"error", Json::string(code)}})); }
bool inside(const std::filesystem::path& value, const std::filesystem::path& parent) {
    return std::mismatch(parent.begin(), parent.end(), value.begin(), value.end()).first == parent.end();
}
Json schema(const std::vector<std::pair<std::string, const char*>>& properties) {
    std::vector<std::pair<std::string, Json>> fields;
    std::vector<Json> required;
    for (const auto& [name, type] : properties) {
        auto value = Json::object({{"type", Json::string(type)}});
        if (std::string_view(type) == "array")
            value = Json::parse(R"({"type":"array","items":{"type":"string"}})");
        fields.emplace_back(name, std::move(value)); required.push_back(Json::string(name));
    }
    fields.emplace_back("instruction_token", Json::parse(R"({"type":"string"})"));
    required.push_back(Json::string("instruction_token"));
    return Json::object({{"type", Json::string("object")}, {"properties", Json::object(fields)},
        {"required", Json::array(required)}, {"additionalProperties", Json::parse("false")}});
}
bool path_argument(const Json& arguments, const char* key) {
    const auto text = arguments.text(key);
    return !text.empty() && text.size() <= 4096 && text.find('\0') == std::string::npos &&
        !local::protected_path(std::filesystem::path(text)) && arguments.text("instruction_token").size() <= 64;
}
bool append(std::vector<Json>& entries, Json value, std::size_t& bytes) {
    const auto size = value.dump().size();
    if (size > 64 * 1024 - bytes) return false;
    bytes += size; entries.push_back(std::move(value)); return true;
}
class Host {
    AgentSettings settings_;
    ContextEpoch epoch_;
    ProcessRunner processes_;
    std::filesystem::path auth_;
public:
    Host(AgentSettings settings, ContextEpoch epoch, const std::filesystem::path& session,
         const std::filesystem::path& auth, const std::filesystem::path& worker)
        : settings_(std::move(settings)), epoch_(std::move(epoch)), processes_(session / "operations", worker),
          auth_(std::filesystem::weakly_canonical(auth)) {}
    std::filesystem::path path(const std::string& text) const {
        require(!text.empty() && text.size() <= 4096 && text.find('\0') == std::string::npos,
                "file_path", Stage::execution);
        const auto input = std::filesystem::path(text).is_absolute() ? std::filesystem::path(text) : settings_.cwd / text;
        require(!local::protected_path(input), "protected_file", Stage::execution);
        const auto resolved = std::filesystem::canonical(input);
        require(!local::protected_path(resolved) && !inside(resolved, auth_) &&
                !inside(resolved, settings_.directory / "auth") && !inside(resolved, settings_.directory / "sessions"),
                "protected_file", Stage::execution);
        return resolved;
    }
    std::optional<ToolOutcome> instructions(const std::filesystem::path& directory, const Json& arguments) const {
        std::vector<Json> complete, added;
        std::size_t bytes = 0;
        for (const auto& file : load_instructions(directory, settings_.home)) {
            auto value = Json::object({{"path", Json::string(file.path.string())}, {"text", Json::string(file.text)}});
            complete.push_back(value);
            const auto base = epoch_.exact_state.at("instructions").elements();
            if (std::find(base.begin(), base.end(), value) == base.end()) {
                require(file.text.size() <= 8192 - bytes, "instruction_size_limit", Stage::execution);
                bytes += file.text.size(); added.push_back(std::move(value));
            }
        }
        const auto token = sha256(Json::array(complete).dump());
        if (arguments.text("instruction_token") == token) return {};
        return result(Json::object({{"status", Json::string("instructions_required")},
            {"executed", Json::parse("false")}, {"instruction_token", Json::string(token)},
            {"instructions", Json::array(added)}}));
    }
    ToolOutcome list(const Json& arguments, std::stop_token stop) const {
        const auto directory = path(arguments.text("path"));
        if (const auto gate = instructions(directory, arguments)) return *gate;
        std::vector<Json> entries;
        std::size_t bytes = 0;
        bool truncated = false;
        for (const auto& entry : std::filesystem::directory_iterator(directory)) {
            if (stop.stop_requested()) return error("cancelled");
            if (entries.size() == 128) { truncated = true; break; }
            const auto name = entry.path().filename().string();
            if (local::protected_path(entry.path())) continue;
            if (!append(entries, Json::object({{"name", Json::string(local::utf8_text(name))},
                {"directory", boolean(entry.is_directory())}, {"symlink", boolean(entry.is_symlink())}}), bytes)) {
                truncated = true; break;
            }
        }
        return result(Json::object({{"path", Json::string(directory.string())}, {"entries", Json::array(entries)},
            {"truncated", boolean(truncated)}}));
    }
    ToolOutcome read(const Json& arguments, std::stop_token stop) const {
        const auto file = path(arguments.text("path"));
        if (const auto gate = instructions(file.parent_path(), arguments)) return *gate;
        const auto offset = arguments.index("offset"), limit = arguments.index("max_bytes");
        require(offset <= 64 * 1024 * 1024 && limit > 0 && limit <= 8192, "file_read_limit", Stage::execution);
        local::Fd fd = local::open_nofollow(file, O_RDONLY | O_NONBLOCK);
        struct stat status{};
        require(fd.get() >= 0 && ::fstat(fd.get(), &status) == 0 && S_ISREG(status.st_mode) && status.st_nlink == 1,
                "file_not_regular", Stage::execution);
        if (stop.stop_requested()) return error("cancelled");
        std::string content(limit + 1, '\0');
        ssize_t count;
        do { count = ::pread(fd.get(), content.data(), content.size(), static_cast<off_t>(offset)); } while (count < 0 && errno == EINTR);
        require(count >= 0, "file_read_failed", Stage::execution);
        const bool truncated = static_cast<std::size_t>(count) > limit;
        content.resize(std::min(limit, static_cast<std::size_t>(count)));
        if (content.find('\0') != std::string::npos) return error("binary_file");
        return result(Json::object({{"path", Json::string(file.string())}, {"offset", number(offset)},
            {"next_offset", number(offset + content.size())}, {"content", Json::string(local::utf8_text(content))},
            {"truncated", boolean(truncated)}}));
    }
    ToolOutcome search(const Json& arguments, std::stop_token stop) const {
        const auto root = path(arguments.text("path"));
        if (const auto gate = instructions(root, arguments)) return *gate;
        const auto query = arguments.text("query"), mode = arguments.text("mode");
        const auto maximum = arguments.index("max_results");
        require(!query.empty() && query.size() <= 256 && (mode == "name" || mode == "content") &&
                maximum > 0 && maximum <= 50, "file_search_arguments", Stage::execution);
        std::vector<Json> matches, scopes;
        std::size_t visited = 0, scanned = 0, bytes = 0;
        bool truncated = false;
        const auto flags = std::filesystem::directory_options::skip_permission_denied;
        for (std::filesystem::recursive_directory_iterator it(root, flags), end; it != end; ++it) {
            if (stop.stop_requested()) return error("cancelled");
            if (++visited > 4096 || scanned >= 256 || matches.size() >= maximum) { truncated = true; break; }
            const auto name = it->path().filename().string();
            if (local::protected_path(it->path()) ||
                inside(it->path(), auth_) || inside(it->path(), settings_.directory / "auth") ||
                inside(it->path(), settings_.directory / "sessions") ||
                std::find(settings_.exclude.begin(), settings_.exclude.end(), name) != settings_.exclude.end()) {
                if (it->is_directory()) it.disable_recursion_pending();
                continue;
            }
            if (it->is_symlink()) { if (it->is_directory()) it.disable_recursion_pending(); continue; }
            if (it->is_directory()) {
                if (std::filesystem::exists(it->path() / "AGENTS.md")) {
                    if (scopes.size() < 50 && !append(scopes, Json::string(it->path().string()), bytes)) truncated = true;
                    it.disable_recursion_pending();
                }
                continue;
            }
            if (!it->is_regular_file()) continue;
            const auto file = path(it->path().string());
            if (mode == "name") {
                if (name.find(query) != std::string::npos)
                    if (!append(matches, Json::object({{"path", Json::string(local::utf8_text(file.string()))}}), bytes)) {
                        truncated = true; break;
                    }
                continue;
            }
            ++scanned;
            local::Fd fd = local::open_nofollow(file, O_RDONLY | O_NONBLOCK);
            struct stat status{};
            if (fd.get() < 0 || ::fstat(fd.get(), &status) != 0 || !S_ISREG(status.st_mode) || status.st_nlink != 1) continue;
            std::string contents(65537, '\0');
            const auto count = ::read(fd.get(), contents.data(), contents.size());
            if (count <= 0) continue;
            truncated = truncated || count > 65536;
            contents.resize(std::min<std::size_t>(65536, static_cast<std::size_t>(count)));
            if (contents.find('\0') != std::string::npos) continue;
            std::size_t start = 0, line = 1;
            while (start < contents.size() && matches.size() < maximum) {
                const auto next = contents.find('\n', start);
                const auto text = std::string_view(contents).substr(start, next == std::string::npos ? next : next - start);
                if (text.find(query) != std::string_view::npos)
                    if (!append(matches, Json::object({{"path", Json::string(local::utf8_text(file.string()))},
                        {"line", number(line)}, {"text", Json::string(local::utf8_text(text.substr(0, 256)))}}), bytes)) {
                        truncated = true; break;
                    }
                if (next == std::string::npos) break;
                start = next + 1; ++line;
            }
        }
        return result(Json::object({{"matches", Json::array(matches)}, {"instruction_scopes", Json::array(scopes)},
            {"truncated", boolean(truncated)}, {"files_scanned", number(scanned)}}));
    }
    ToolOutcome run(const ToolInvocation& invocation, std::stop_token stop) const {
        const auto& arguments = invocation.arguments;
        const auto cwd = path(arguments.text("cwd"));
        if (const auto gate = instructions(cwd, arguments)) return *gate;
        ProcessOptions options{cwd, {}, arguments.index("timeout_ms"), settings_.process_output_bytes};
        require(options.timeout_ms > 0 && options.timeout_ms <= settings_.process_timeout_ms, "process_timeout_limit", Stage::execution);
        for (const auto& argument : arguments.at("argv").elements()) options.argv.push_back(argument.text());
        return processes_.run(invocation, options, stop);
    }
    std::optional<ToolOutcome> recover(const ToolInvocation& invocation) const { return processes_.recover(invocation); }
};
}
std::shared_ptr<const ToolSnapshot> local_tools(const AgentSettings& settings, const ContextEpoch& epoch,
    const std::filesystem::path& session, const std::filesystem::path& auth, const std::filesystem::path& worker) {
    auto host = std::make_shared<Host>(settings, epoch, session, auth, worker);
    ToolRegistry registry;
    auto add = [&](std::string name, std::string description, Json parameters,
                   Validator validator, std::function<ToolOutcome(const Json&, std::stop_token)> action) {
        registry.register_tool({{std::move(name), std::move(description), "ansa_local_v1", std::move(parameters)},
            std::move(validator), [action = std::move(action)](const Json& arguments, std::stop_token stop) {
                try { return action(arguments, stop); }
                catch (const Error& failure) { return error(failure.diagnostic.c_str()); }
                catch (const std::filesystem::filesystem_error&) { return error("file_access_failed"); }
            }, {}, {}, false, true});
    };
    add("list_directory", "List up to 128 directory entries. instruction_token identifies applicable AGENTS.md instructions.",
        schema({{"path", "string"}}), [](const Json& args) { return path_argument(args, "path"); },
        [host](const Json& args, std::stop_token stop) { return host->list(args, stop); });
    add("read_file", "Read a regular UTF-8 file by byte offset, max_bytes 1..8192. Dotenv and credential stores are excluded.",
        schema({{"path", "string"}, {"offset", "integer"}, {"max_bytes", "integer"}}),
        [](const Json& args) { return path_argument(args, "path") && args.index("offset") <= 64 * 1024 * 1024 &&
            args.index("max_bytes") > 0 && args.index("max_bytes") <= 8192; },
        [host](const Json& args, std::stop_token stop) { return host->read(args, stop); });
    add("search_files", "Bounded literal search: mode=name or content, max_results 1..50. Nested instruction scopes are returned separately.",
        schema({{"path", "string"}, {"query", "string"}, {"mode", "string"}, {"max_results", "integer"}}),
        [](const Json& args) { return path_argument(args, "path") && !args.text("query").empty() && args.text("query").size() <= 256 &&
            (args.text("mode") == "content" || args.text("mode") == "name") && args.index("max_results") > 0 && args.index("max_results") <= 50; },
        [host](const Json& args, std::stop_token stop) { return host->search(args, stop); });
    ToolDefinition process{{"run_command", "Run argv without a shell unless explicitly requested. Noninteractive, no sudo, no background services. "
        "timeout_ms must be positive and not exceed the configured limit. Exit codes and truncated stdout/stderr are returned as data.",
        "ansa_process_v1", schema({{"argv", "array"}, {"cwd", "string"}, {"timeout_ms", "integer"}})},
        [maximum = settings.process_timeout_ms](const Json& args) {
            if (!path_argument(args, "cwd") || !args.index("timeout_ms") || args.index("timeout_ms") > maximum) return false;
            const auto argv = args.at("argv").elements();
            if (argv.empty() || argv.size() > 64 || argv.front().text().empty()) return false;
            std::size_t bytes = 0;
            for (const auto& argument : argv) {
                const auto value = argument.text();
                if (value.find('\0') != std::string::npos || value.size() > 16384 - bytes) return false;
                bytes += value.size();
            }
            return true;
        }, {}};
    process.invoke = [host](const ToolInvocation& invocation, std::stop_token stop) {
        try { return host->run(invocation, stop); }
        catch (const std::filesystem::filesystem_error&) { return error("process_cwd_unavailable"); }
        // Validation errors before ProcessRunner launches anything are ordinary tool errors.
        catch (const Error& failure) {
            if (failure.diagnostic == "process_timeout_limit" || failure.diagnostic == "protected_file" ||
                failure.diagnostic == "file_path" || failure.diagnostic == "instruction_size_limit")
                return error(failure.diagnostic.c_str());
            throw;
        }
    };
    process.recover = [host](const ToolInvocation& invocation) { return host->recover(invocation); };
    process.requires_journal = true;
    registry.register_tool(std::move(process));
    registry.register_tool(test_value_tool());
    return registry.snapshot();
}
}
