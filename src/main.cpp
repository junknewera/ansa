#include "omh/offline.hpp"
#include "omh/runtime.hpp"
#include "omh/transport.hpp"
#include "omh/auth.hpp"
#include <iostream>
#include <charconv>
#include <set>

namespace omh {
int offline_cli(const std::filesystem::path&);
int auth_cli(std::string_view, const std::filesystem::path&);
int live_cli(const std::filesystem::path&, const std::filesystem::path&,
             const std::string&, const std::string&, std::size_t);
}

namespace {
omh::Json demo() {
    using namespace omh;
    ToolRegistry registry;
    registry.register_tool(test_value_tool());
    Runtime runtime({"offline"}, {"demo_epoch", "Use the pure fixture tool", Json::parse(R"({"mode":"offline"})")},
                    registry.snapshot(), offline_generate);
    const auto posted = runtime.post({1, {"offline"}, {"demo"}, runtime.incarnation(), {"demo_submit"},
                                      Submit{"Call omh.get_test_value"}});
    require(std::get_if<PostStatus>(&posted) && std::get<PostStatus>(posted) == PostStatus::accepted, "demo_submit");
    runtime.close_input();

    std::optional<FrontendSnapshot> final;
    while (const auto event = runtime.next_event()) {
        if (const auto* snapshot = std::get_if<FrontendSnapshot>(&event->payload)) final = *snapshot;
    }
    require(final && final->state == "completed" && final->completed_tools == 1 &&
            final->text == "OMH_TOOL_OK:OMH_FIXED_VALUE_42", "demo_roundtrip");
    return Json::object({{"status", Json::string("passed")}, {"mode", Json::string("offline")},
        {"tool_invocations", Json::parse("1")}, {"native_replay", Json::parse("true")},
        {"final_text", Json::string(final->text)}});
}
} // namespace
int main(int argc, char** argv) {
    try {
        const std::string_view command = argc > 1 ? argv[1] : "--self-check";
        if (command == "--live-jsonl") {
            auto auth_directory = omh::default_auth_directory();
            std::filesystem::path session;
            std::string model = "gpt-6.1-sol", returned_model;
            std::size_t compact_bytes = 256 * 1024;
            std::set<std::string_view> seen;
            for (int i = 2; i < argc; i += 2) {
                omh::require(i + 1 < argc && seen.insert(argv[i]).second, "live_cli_options", omh::Stage::frontend);
                const std::string_view key = argv[i], value = argv[i + 1];
                if (key == "--session") session = value;
                else if (key == "--auth-dir") auth_directory = value;
                else if (key == "--model") model = value;
                else if (key == "--returned-model") returned_model = value;
                else if (key == "--compact-bytes") {
                    const auto parsed = std::from_chars(value.data(), value.data() + value.size(), compact_bytes);
                    omh::require(parsed.ec == std::errc{} && parsed.ptr == value.data() + value.size(),
                                 "live_cli_options", omh::Stage::frontend);
                } else omh::fail(omh::ErrorCode::contract, omh::Stage::frontend, "live_cli_options");
            }
            omh::require(!session.empty(), "live_session_required", omh::Stage::frontend);
            return omh::live_cli(session, auth_directory, model,
                                 returned_model.empty() ? model : returned_model, compact_bytes);
        }
        if (command == "--login" || command == "--login-consent" || command == "--logout" || command == "--auth-status" ||
            command == "--refresh-auth" || command == "--models") {
            if (argc == 2) return omh::auth_cli(command, omh::default_auth_directory());
            if (argc == 4 && std::string_view(argv[2]) == "--auth-dir") return omh::auth_cli(command, argv[3]);
            std::cerr << "Usage: omh " << command << " [--auth-dir PATH]\n";
            return 2;
        }
        if (argc == 4 && command == "--offline-jsonl" && std::string_view(argv[2]) == "--session")
            return omh::offline_cli(argv[3]);
        if (argc > 2 || (command != "--self-check" && command != "--offline-demo" && command != "--offline-jsonl")) {
            std::cerr << "Usage: omh [--self-check|--offline-demo|--offline-jsonl [--session PATH]]\n";
            return 2;
        }
        if (command == "--offline-jsonl") return omh::offline_cli({});
        if (command == "--offline-demo") std::cout << demo().dump() << '\n';
        else {
            const auto info = omh::dependency_smoke();
            std::cout << omh::Json::object({{"project", omh::Json::string("oh-my-harness")},
                {"target", omh::Json::string(info.target)}, {"curl", omh::Json::string(info.curl)},
                {"tls", omh::Json::string(info.tls)}, {"yyjson", omh::Json::string(info.yyjson)},
                {"openssl", omh::Json::string(info.openssl)}}).dump() << '\n';
        }
        return 0;
    } catch (const omh::Error& error) { std::cerr << omh::diagnostic_json(error).dump() << '\n'; return 1; }
    catch (...) { std::cerr << "{\"diagnostic\":\"internal_failure\"}\n"; return 1; }
}
