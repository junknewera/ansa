#include "omh/auth.hpp"
#include <fcntl.h>
#include <cerrno>
#include <chrono>
#include <iostream>
#include <spawn.h>
#include <sys/wait.h>
#include <unistd.h>
#include <thread>

extern char** environ;
namespace omh {
namespace {
void open_browser(std::string_view url) {
    require(url.find("id_token_hint") == std::string_view::npos, "auth_browser_hint", Stage::frontend);
    std::cerr << url << '\n';
    std::string address(url);
    posix_spawn_file_actions_t actions;
    require(posix_spawn_file_actions_init(&actions) == 0, "auth_browser_spawn", Stage::frontend);
    posix_spawn_file_actions_addopen(&actions, STDOUT_FILENO, "/dev/null", O_WRONLY, 0);
    posix_spawn_file_actions_addopen(&actions, STDERR_FILENO, "/dev/null", O_WRONLY, 0);
    char command[] = "xdg-open";
    char* arguments[]{command, address.data(), nullptr};
    pid_t child = -1;
    const int started = ::posix_spawnp(&child, command, &actions, nullptr, arguments, environ);
    posix_spawn_file_actions_destroy(&actions);
    if (started != 0) return;
    // A launcher can remain alive for the browser window's lifetime.
    // Reaping it must not delay the callback listener.
    std::thread([child] {
        int status = 0;
        while (::waitpid(child, &status, 0) < 0 && errno == EINTR) {}
    }).detach();
}
Json number(std::uint64_t n) { return Json::parse(std::to_string(n)); }
}
int auth_cli(std::string_view command, const std::filesystem::path& directory) {
    AuthManager manager(directory);
    if (command == "--login" || command == "--login-consent") {
        std::cerr << "Continue with ChatGPT в открывшемся браузере.\n";
        manager.login(open_browser, {}, command == "--login-consent");
        std::cout << Json::object({{"status", Json::string("signed_in")},
            {"plan_enabled", Json::parse(manager.status().plan_enabled ? "true" : "false")}}).dump() << '\n';
    } else if (command == "--logout") {
        const bool revoked = manager.logout();
        std::cout << Json::object({{"status", Json::string("signed_out")},
            {"remote_revocation_confirmed", Json::parse(revoked ? "true" : "false")}}).dump() << '\n';
    } else if (command == "--refresh-auth") {
        (void)manager.access({}, true);
        std::cout << R"({"status":"refreshed"})" << '\n';
    } else if (command == "--models") {
        const auto catalog = manager.models();
        std::vector<Json> listed;
        for (const auto& model : catalog.at("models").elements())
            if (model.text("visibility") == "list")
                listed.push_back(Json::object({{"slug", model.at("slug")}, {"display_name", model.at("display_name")}}));
        std::cout << Json::array(listed).dump() << '\n';
    } else {
        const auto status = manager.status();
        std::cout << Json::object({{"signed_in", Json::parse(status.logged_in ? "true" : "false")},
            {"refresh_pending", Json::parse(status.refresh_pending ? "true" : "false")},
            {"plan_enabled", Json::parse(status.plan_enabled ? "true" : "false")},
            {"account", Json::string(status.account)}, {"expires_in", number(status.expires_in)},
            {"refresh_wait", number(status.refresh_wait)}}).dump() << '\n';
    }
    return 0;
}
} // namespace omh
