#pragma once
#include "omh/contracts.hpp"
#include <filesystem>

namespace omh {
struct AgentSettings {
    std::filesystem::path cwd, home, directory;
    std::string model = "gpt-6.1-sol", returned_model = "gpt-6.1-sol";
    std::size_t compact_bytes = 256 * 1024, tool_rounds = 32, tool_calls = 64;
    std::size_t process_timeout_ms = 60000, process_output_bytes = 16384;
    std::vector<std::string> exclude{".git", "node_modules", ".cache", "build", "build-ansa", "result"};
    Json effective() const;
};
struct InstructionFile { std::filesystem::path path; std::string text; };
AgentSettings load_agent_settings(const std::filesystem::path& cwd, const std::filesystem::path& home);
std::vector<InstructionFile> load_instructions(const std::filesystem::path& directory,
    const std::filesystem::path& home, std::size_t limit = 64 * 1024);
ContextEpoch agent_epoch(const AgentSettings&);

struct SelectedSession { SessionId id; std::filesystem::path directory; };
SelectedSession select_session(const AgentSettings&, bool resume_last, const std::string& explicit_id,
    std::string_view profile = "live");
}
