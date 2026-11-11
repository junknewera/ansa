#pragma once
#include "omh/agent_config.hpp"
#include "omh/process.hpp"

namespace omh {
std::shared_ptr<const ToolSnapshot> local_tools(const AgentSettings&, const ContextEpoch&,
    const std::filesystem::path& session, const std::filesystem::path& auth_directory,
    const std::filesystem::path& worker = "/proc/self/exe");
}
