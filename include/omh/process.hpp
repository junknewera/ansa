#pragma once
#include "omh/tools.hpp"
#include <filesystem>

namespace omh {
struct ProcessOptions {
    std::filesystem::path cwd;
    std::vector<std::string> argv;
    std::size_t timeout_ms = 60000;
    std::size_t output_bytes = 16384;
};
class ProcessRunner {
    std::filesystem::path root_, worker_;
public:
    explicit ProcessRunner(std::filesystem::path evidence, std::filesystem::path worker = "/proc/self/exe");
    ToolOutcome run(const ToolInvocation&, const ProcessOptions&, std::stop_token) const;
    std::optional<ToolOutcome> recover(const ToolInvocation&) const;
};
// Internal subprocess entry point. Does not initialize auth, runtime or frontend.
int process_worker(const std::filesystem::path& job);
}
