#include "omh/agent_config.hpp"
#include "omh/auth.hpp"
#include "omh/contracts.hpp"
#include "omh/local_storage.hpp"
#include <algorithm>
#include <cerrno>
#include <chrono>
#include <fcntl.h>
#include <sys/stat.h>
#include <unistd.h>

namespace omh {
namespace {
Json number(std::size_t value) { return Json::parse(std::to_string(value)); }
std::optional<std::string> text_file(const std::filesystem::path& path, std::size_t limit) {
    require(!local::protected_path(path), "protected_instruction_path", Stage::frontend);
    // Automatic prompt/config loading must not follow links into credential stores.
    // Walk every parent with nofollow as well; checking only the final name is insufficient.
    local::Fd file = local::open_nofollow(std::filesystem::absolute(path), O_RDONLY | O_NONBLOCK);
    if (file.get() < 0 && errno == ENOENT) return {};
    struct stat status{};
    require(file.get() >= 0 && ::fstat(file.get(), &status) == 0 && S_ISREG(status.st_mode) && status.st_nlink == 1,
            "config_file_type", Stage::frontend);
    std::string out;
    char bytes[4096];
    for (;;) {
        const auto count = ::read(file.get(), bytes, sizeof(bytes));
        if (count < 0 && errno == EINTR) continue;
        require(count >= 0, "config_file_read", Stage::frontend);
        if (!count) break;
        require(static_cast<std::size_t>(count) <= limit - out.size(), "instruction_size_limit", Stage::frontend);
        out.append(bytes, static_cast<std::size_t>(count));
    }
    (void)Json::string(out);
    return out;
}
void overlay(AgentSettings& settings, const std::filesystem::path& path, bool project) {
    const auto bytes = text_file(path, 64 * 1024);
    if (!bytes) return;
    const auto json = Json::parse(*bytes, 64 * 1024, 16, 4096);
    require(json.is_object() && json.index("schema_version") == 1, "config_schema_version", Stage::frontend);
    for (const auto& key : json.keys()) {
        if (key == "schema_version") continue;
        if (key == "model") {
            settings.model = json.text("model");
            if (!json.has("returned_model")) settings.returned_model = settings.model;
        }
        else if (key == "returned_model") settings.returned_model = json.text("returned_model");
        else if (key == "compact_bytes") settings.compact_bytes = json.index("compact_bytes");
        else if (key == "tool_rounds") settings.tool_rounds = json.index("tool_rounds");
        else if (key == "tool_calls") settings.tool_calls = json.index("tool_calls");
        else if (key == "process_timeout_ms") settings.process_timeout_ms = json.index("process_timeout_ms");
        else if (key == "process_output_bytes") settings.process_output_bytes = json.index("process_output_bytes");
        else if (key == "exclude") {
            const auto entries = json.at(key).elements();
            require(entries.size() <= 64, "config_excludes_limit", Stage::frontend);
            settings.exclude.clear();
            for (const auto& entry : entries) {
                auto name = entry.text();
                require(!name.empty() && name.size() <= 128 && name.find('/') == std::string::npos &&
                        name.find('\0') == std::string::npos, "config_exclude", Stage::frontend);
                settings.exclude.push_back(std::move(name));
            }
        } else if (key == "access_mode" || key == "allow_sudo") {
            require(!project, "project_security_setting", Stage::frontend);
            require(key == "access_mode" ? json.text("access_mode") == "full_access" :
                    json.at("allow_sudo") == Json::parse("false"), "unsupported_access_policy", Stage::frontend);
        } else fail(ErrorCode::contract, Stage::frontend, "unknown_config_setting");
    }
}
}
Json AgentSettings::effective() const {
    std::vector<Json> excludes;
    for (const auto& name : exclude) excludes.push_back(Json::string(name));
    return Json::object({{"schema_version", number(1)}, {"cwd", Json::string(cwd.string())},
        {"model", Json::string(model)}, {"returned_model", Json::string(returned_model)},
        {"compact_bytes", number(compact_bytes)}, {"tool_rounds", number(tool_rounds)},
        {"tool_calls", number(tool_calls)}, {"process_timeout_ms", number(process_timeout_ms)},
        {"process_output_bytes", number(process_output_bytes)}, {"exclude", Json::array(excludes)},
        {"access_mode", Json::string("full_access")}, {"allow_sudo", Json::parse("false")}});
}
AgentSettings load_agent_settings(const std::filesystem::path& cwd, const std::filesystem::path& home) {
    AgentSettings settings;
    settings.cwd = std::filesystem::canonical(cwd);
    settings.home = std::filesystem::canonical(home);
    settings.directory = settings.home / ".ansa";
    overlay(settings, settings.directory / "config.json", false);
    if (settings.cwd != settings.home) overlay(settings, settings.cwd / ".ansa" / "config.json", true);
    require(!settings.model.empty() && settings.model.size() <= 256 && !settings.returned_model.empty() &&
            settings.returned_model.size() <= 256 && settings.compact_bytes >= 256 && settings.compact_bytes <= 4 * 1024 * 1024 &&
            settings.tool_rounds > 0 && settings.tool_rounds <= 64 && settings.tool_calls > 0 && settings.tool_calls <= 256 &&
            settings.process_timeout_ms > 0 && settings.process_timeout_ms <= 600000 &&
            settings.process_output_bytes > 0 && settings.process_output_bytes <= 16384,
            "agent_config_limits", Stage::frontend);
    return settings;
}
std::vector<InstructionFile> load_instructions(const std::filesystem::path& directory,
    const std::filesystem::path& home, std::size_t limit) {
    std::vector<InstructionFile> out;
    auto add = [&](const std::filesystem::path& path) {
        if (const auto text = text_file(path, limit)) {
            require(text->size() <= limit, "instruction_size_limit", Stage::frontend);
            limit -= text->size(); out.push_back({path, *text});
        }
    };
    add(home / ".ansa" / "AGENTS.md");
    std::vector<std::filesystem::path> parents;
    auto current = std::filesystem::canonical(directory);
    for (;;) {
        require(parents.size() < 128, "instruction_depth_limit", Stage::frontend);
        parents.push_back(current);
        if (current == current.root_path()) break;
        current = current.parent_path();
    }
    for (auto it = parents.rbegin(); it != parents.rend(); ++it) add(*it / "AGENTS.md");
    return out;
}
ContextEpoch agent_epoch(const AgentSettings& settings) {
    const std::string base = "You are ansa, a local general-purpose agent. Solve the user's task through the advertised tools. "
        "The working directory is context, not a Git requirement or a filesystem sandbox. Use absolute paths or paths relative to it. "
        "Make multiple tool rounds as needed. A process exit code or a tool error is information: inspect it and choose the next step. "
        "Do not repeat operations with unknown effects. Sudo and privilege elevation through exec are disabled by the backend. "
        "Do not ask for or handle sudo passwords. Do not read dotenv files or agent credential stores. "
        "Respect applicable AGENTS.md instructions; directory-specific instructions are also returned by file tools. "
        "Never infer configuration or permissions from AGENTS.md: configuration is validated separately. "
        "Tools are non-interactive; never start background services as a substitute for a managed tool. "
        "Keep user-facing answers concise and use the user's language.";
    auto instructions = base;
    std::vector<Json> files;
    for (const auto& file : load_instructions(settings.cwd, settings.home)) {
        instructions += "\n\nInstructions from " + file.path.string() + ":\n" + file.text;
        files.push_back(Json::object({{"path", Json::string(file.path.string())}, {"text", Json::string(file.text)}}));
    }
    instructions += "\nFor tools in the initial working directory the instruction_token is " + sha256(Json::array(files).dump()) +
        ". If a tool returns instructions_required, read the instructions and repeat with its instruction_token. "
        "Use file tools to discover directory-specific instructions before operating in other directories.";
    return {"ansa_local_v1", std::move(instructions),
        Json::object({{"settings", settings.effective()}, {"instructions", Json::array(files)},
                      {"compaction", Json::string("prompt_summary_keep_two_v1")}})};
}
SelectedSession select_session(const AgentSettings& settings, bool resume_last, const std::string& explicit_id, std::string_view profile) {
    require(!resume_last || explicit_id.empty(), "session_selection_conflict", Stage::frontend);
    const auto root_path = settings.directory / "sessions";
    local::private_directory(root_path);
    auto root = local::open_private_directory(root_path);
    auto info = [&](const std::string& id) {
        require(valid_id(id), "invalid_session_id", Stage::frontend);
        auto directory = local::open_private_directory(root_path / id);
        const auto metadata = local::read_json(directory.get(), "info.json", 4096);
        require(metadata && metadata->index("schema_version") == 1 && metadata->text("id") == id &&
                metadata->text("cwd") == settings.cwd.string() && metadata->text("profile") == profile,
                "session_workspace_mismatch", Stage::frontend);
        return SelectedSession{{id}, root_path / id};
    };
    if (!explicit_id.empty()) return info(explicit_id);
    if (resume_last) {
        std::optional<std::pair<std::filesystem::file_time_type, std::string>> latest;
        std::size_t count = 0;
        for (const auto& entry : std::filesystem::directory_iterator(root_path)) {
            require(++count <= 4096, "session_catalog_limit", Stage::frontend);
            const auto id = entry.path().filename().string();
            if (!valid_id(id) || !entry.is_directory() || entry.is_symlink()) continue;
            auto directory = local::open_private_directory(entry.path());
            const auto metadata = local::read_json(directory.get(), "info.json", 4096);
            if (!metadata || metadata->text("cwd") != settings.cwd.string() || metadata->text("profile") != profile) continue;
            auto updated = std::filesystem::file_time_type::min();
            std::size_t parts = 0;
            for (const auto& part : std::filesystem::directory_iterator(entry.path())) {
                require(++parts <= 4100, "session_parts_limit", Stage::frontend);
                if (part.path().filename().string().starts_with("part-") && part.is_regular_file())
                    updated = std::max(updated, part.last_write_time());
            }
            if (updated != std::filesystem::file_time_type::min() && (!latest || updated > latest->first)) latest = {updated, id};
        }
        require(latest.has_value(), "no_previous_session", Stage::frontend);
        return info(latest->second);
    }
    const auto millis = std::chrono::duration_cast<std::chrono::milliseconds>(
        std::chrono::system_clock::now().time_since_epoch()).count();
    const auto id = "s_" + std::to_string(millis) + "_" + random_url_token(16);
    require(valid_id(id) && ::mkdirat(root.get(), id.c_str(), 0700) == 0, "session_directory_create", Stage::frontend);
    local::sync(root.get());
    auto directory = local::open_private_directory(root_path / id);
    local::create_json(directory.get(), "info.json", Json::object({{"schema_version", number(1)},
        {"id", Json::string(id)}, {"cwd", Json::string(settings.cwd.string())}, {"profile", Json::string(profile)}}));
    return {{id}, root_path / id};
}
}
