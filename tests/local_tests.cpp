#include "omh/agent_config.hpp"
#include "omh/local_tools.hpp"
#include "omh/local_storage.hpp"
#include "omh/session.hpp"
#include "omh/journal.hpp"
#include <fstream>
#include <iostream>
#include <sys/stat.h>
#include <thread>
#include <unistd.h>

namespace {
using namespace omh;
void check(bool value, const char* text) { if (!value) throw std::runtime_error(text); }
template<class T> T unwrap(Result<T> value) {
    if (auto* error = std::get_if<Error>(&value)) throw *error;
    return std::get<T>(std::move(value));
}
template<class F> void rejects(F action) {
    try { action(); } catch (const Error&) { return; }
    throw std::runtime_error("expected rejection");
}
struct Temp {
    std::filesystem::path root, home, cwd;
    Temp() {
        char name[] = "/tmp/ansa-local-XXXXXX";
        const auto path = ::mkdtemp(name); check(path, "mkdtemp"); root = path;
        home = root / "home"; cwd = home / "plain";
        std::filesystem::create_directories(cwd);
    }
    ~Temp() { std::error_code error; std::filesystem::remove_all(root, error); }
};
void write(const std::filesystem::path& path, std::string_view text) {
    std::filesystem::create_directories(path.parent_path());
    std::ofstream output(path); output << text; check(bool(output), "fixture write");
}
Json number(std::size_t value) { return Json::parse(std::to_string(value)); }
std::string token(const ContextEpoch& epoch) { return sha256(epoch.exact_state.at("instructions").dump()); }
Json invoke(std::shared_ptr<const ToolSnapshot> tools, const ContextEpoch& epoch, const std::string& name, const Json& args) {
    Request request({}, epoch, tools->advertisement(), {});
    const auto item = Json::object({{"type", Json::string("function_call")}, {"id", Json::string("item")},
        {"call_id", Json::string("call")}, {"name", Json::string(name)}, {"namespace", Json::string("omh")},
        {"arguments", Json::string(args.dump())}, {"status", Json::string("completed")}});
    auto response = load_response(Json::object({{"request", save_request(request)}, {"id", Json::string("response")},
        {"model", Json::string(request.binding().returned_model)}, {"items", Json::array({item})}}));
    OperationIds ids("test"); auto batch = unwrap(PreparedBatch::prepare(response, tools, ids));
    const auto results = unwrap(batch.execute());
    check(results.size() == 1 && results[0].effects == Effects::known_result, "file tool returned unknown effects");
    return Json::parse(results[0].output);
}
void configuration() {
    Temp temp;
    write(temp.home / ".ansa/config.json", R"({"schema_version":1,"model":"global","tool_rounds":12,"allow_sudo":false})");
    write(temp.cwd / ".ansa/config.json", R"({"schema_version":1,"model":"project","tool_calls":24})");
    const auto settings = load_agent_settings(temp.cwd, temp.home);
    check(settings.model == "project" && settings.returned_model == "project" && settings.tool_rounds == 12 &&
          settings.tool_calls == 24, "config precedence/default model mismatch");
    write(temp.cwd / ".ansa/config.json", R"({"schema_version":1,"allow_sudo":false})");
    rejects([&] { load_agent_settings(temp.cwd, temp.home); });
    write(temp.cwd / ".ansa/config.json", R"({"schema_version":1,"unknown":true})");
    rejects([&] { load_agent_settings(temp.cwd, temp.home); });
    write(temp.cwd / ".ansa/config.json", R"({"schema_version":1,"process_timeout_ms":0})");
    rejects([&] { load_agent_settings(temp.cwd, temp.home); });
    write(temp.cwd / ".ansa/config.json", R"({"schema_version":1})");
    write(temp.home / ".ansa/config.json", R"({"schema_version":1,"allow_sudo":true})");
    rejects([&] { load_agent_settings(temp.cwd, temp.home); });
}
void instruction_links() {
    Temp temp;
    const auto private_file = temp.home / ".ansa/auth/credentials.json";
    write(private_file, R"({"synthetic_secret":"must_never_enter_prompt"})");
    std::filesystem::create_symlink(private_file, temp.cwd / "AGENTS.md");
    rejects([&] { (void)load_instructions(temp.cwd, temp.home); });
    std::filesystem::remove(temp.cwd / "AGENTS.md");
    std::filesystem::create_hard_link(private_file, temp.cwd / "AGENTS.md");
    rejects([&] { (void)load_instructions(temp.cwd, temp.home); });
    std::filesystem::remove(temp.cwd / "AGENTS.md");
    std::filesystem::create_directory_symlink(temp.home / ".ansa/auth", temp.cwd / ".ansa");
    rejects([&] { (void)load_agent_settings(temp.cwd, temp.home); });
}
void files() {
    Temp temp;
    write(temp.home / ".ansa/AGENTS.md", "Global rule MARKER_GLOBAL");
    write(temp.cwd / "AGENTS.md", "Project rule MARKER_PROJECT");
    write(temp.cwd / "value.txt", "line one\nneedle line\n");
    write(temp.cwd / "nested/AGENTS.md", "Nested rule MARKER_NESTED");
    write(temp.cwd / "nested/hidden.txt", "nested needle");
    const auto settings = load_agent_settings(temp.cwd, temp.home);
    const auto epoch = agent_epoch(settings);
    check(epoch.instructions.find("MARKER_GLOBAL") != std::string::npos && epoch.instructions.find("MARKER_PROJECT") != std::string::npos &&
          epoch.instructions.find("MARKER_NESTED") == std::string::npos, "instruction scopes flattened");
    auto tools = local_tools(settings, epoch, temp.root / "session", temp.root / "auth");
    const auto readonly = tools->read_only();
    check(readonly->advertisement().descriptors.size() == 4, "read-only tool count");
    for (const auto& descriptor : readonly->advertisement().descriptors)
        check(descriptor.name != "run_command", "process in read-only snapshot");
    const auto args = Json::object({{"path", Json::string("value.txt")}, {"offset", number(0)}, {"max_bytes", number(8192)},
        {"instruction_token", Json::string(token(epoch))}});
    check(invoke(tools, epoch, "read_file", args).text("content") == "line one\nneedle line\n", "file read");
    auto nested = Json::object({{"path", Json::string("nested/hidden.txt")}, {"offset", number(0)}, {"max_bytes", number(8192)},
        {"instruction_token", Json::string(token(epoch))}});
    const auto required = invoke(tools, epoch, "read_file", nested);
    check(required.text("status") == "instructions_required" && required.at("instructions").dump().find("MARKER_NESTED") != std::string::npos,
          "nested instructions ignored");
    nested = Json::object({{"path", Json::string("nested/hidden.txt")}, {"offset", number(0)}, {"max_bytes", number(8192)},
        {"instruction_token", required.at("instruction_token")}});
    check(invoke(tools, epoch, "read_file", nested).text("content") == "nested needle", "instruction acknowledgement failed");
    const auto search = invoke(tools, epoch, "search_files", Json::object({{"path", Json::string(".")}, {"query", Json::string("needle")},
        {"mode", Json::string("content")}, {"max_results", number(10)}, {"instruction_token", Json::string(token(epoch))}}));
    check(search.at("matches").elements().size() == 1 && search.at("instruction_scopes").elements().size() == 1,
          "search crossed nested instruction scope");
    check(::mkfifo((temp.cwd / "fifo").c_str(), 0600) == 0, "mkfifo");
    const auto fifo = invoke(tools, epoch, "read_file", Json::object({{"path", Json::string("fifo")}, {"offset", number(0)},
        {"max_bytes", number(1024)}, {"instruction_token", Json::string(token(epoch))}}));
    check(fifo.text("error") == "file_not_regular", "FIFO read blocked");
    // Path rejection is tested without creating or opening a dotenv file.
    rejects([&] { invoke(tools, epoch, "read_file", Json::object({{"path", Json::string(".env")}, {"offset", number(0)},
        {"max_bytes", number(1024)}, {"instruction_token", Json::string(token(epoch))}})); });
    write(temp.root / "auth/private", "synthetic fixture");
    std::filesystem::create_symlink(temp.root / "auth/private", temp.cwd / "linked");
    check(invoke(tools, epoch, "read_file", Json::object({{"path", Json::string("linked")}, {"offset", number(0)},
        {"max_bytes", number(1024)}, {"instruction_token", Json::string(token(epoch))}})).text("error") == "protected_file",
          "symlink bypassed credential root exclusion");
    std::filesystem::create_hard_link(temp.root / "auth/private", temp.cwd / "linked-hard");
    check(invoke(tools, epoch, "read_file", Json::object({{"path", Json::string("linked-hard")}, {"offset", number(0)},
        {"max_bytes", number(1024)}, {"instruction_token", Json::string(token(epoch))}})).text("error") == "file_not_regular",
          "hardlink leaked protected content");
    const auto links = invoke(tools, epoch, "search_files", Json::object({{"path", Json::string(".")},
        {"query", Json::string("synthetic fixture")}, {"mode", Json::string("content")}, {"max_results", number(10)},
        {"instruction_token", Json::string(token(epoch))}}));
    check(links.at("matches").elements().empty(), "search leaked hardlinked content");
}
void catalog() {
    Temp temp; const auto settings = load_agent_settings(temp.cwd, temp.home);
    const auto epoch = agent_epoch(settings);
    ToolRegistry tools; tools.register_tool(test_value_tool());
    auto create = [&](std::string_view profile) {
        auto selected = select_session(settings, false, {}, profile);
        SessionState state(selected.id, Request({}, epoch, tools.snapshot()->advertisement(), {}));
        Journal journal(selected.directory, [&](const Json& value) { state.apply(value); });
        journal.append(state.initialization()); return selected;
    };
    const auto first = create("live");
    std::this_thread::sleep_for(std::chrono::milliseconds(2));
    const auto second = create("live");
    (void)create("offline");
    check(first.id != second.id && select_session(settings, true, {}).id == second.id &&
          select_session(settings, false, first.id.value).id == first.id, "session selection");
    auto elsewhere = settings; elsewhere.cwd = temp.root;
    rejects([&] { select_session(elsewhere, false, first.id.value); });
    rejects([&] { select_session(settings, false, "../escape"); });
}
}
int main(int argc, char** argv) {
    try {
        check(argc == 2, "suite required");
        const std::string_view suite = argv[1];
        if (suite == "config") configuration();
        else if (suite == "files") files();
        else if (suite == "catalog") catalog();
        else if (suite == "instruction-links") instruction_links();
        else throw std::runtime_error("unknown suite");
        return 0;
    } catch (const std::exception& error) { std::cerr << error.what() << '\n'; return 1; }
}
