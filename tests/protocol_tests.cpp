#include "omh/contracts.hpp"
#include "omh/tools.hpp"
#include "omh/transport.hpp"
#include <fstream>
#include <iostream>
#include <stdexcept>

namespace {
void check(bool condition, const char* diagnostic) { if (!condition) throw std::runtime_error(diagnostic); }
}
int main(int argc, char** argv) {
    try {
        check(argc == 2, "suite required");
        const auto info = omh::dependency_smoke();
        check(!info.curl.empty() && !info.openssl.empty(), "dependency linkage");
        omh::ToolRegistry registry; registry.register_tool(omh::test_value_tool());
        const auto snapshot = registry.snapshot();
        omh::Request request({}, {"test_epoch", "fixture instructions", omh::Json::parse("{}")},
            snapshot->advertisement(), {omh::Json::parse(R"({"role":"user","content":"fixture"})")});
        std::ifstream fixture(std::string(OMH_FIXTURES) + "/round1.sse", std::ios::binary);
        check(bool(fixture), "fixture missing");
        std::string bytes((std::istreambuf_iterator<char>(fixture)), {});
        auto response = omh::decode_offline(request, {200, {"text/event-stream"}}, {bytes});
        if (const auto* error = std::get_if<omh::Error>(&response)) throw *error;
        check(std::get<omh::CompletedResponse>(response).calls().size() == 1, "fixture call");
        std::cout << argv[1] << ": passed\n";
        return 0;
    } catch (const std::exception& error) { std::cerr << error.what() << '\n'; return 1; }
}
