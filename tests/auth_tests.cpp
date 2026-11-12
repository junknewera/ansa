#include "omh/auth.hpp"
#include "omh/tools.hpp"
#include <curl/curl.h>
#include <openssl/core_names.h>
#include <openssl/evp.h>
#include <openssl/rsa.h>
#include <atomic>
#include <condition_variable>
#include <fstream>
#include <iostream>
#include <map>
#include <netinet/in.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <thread>
#include <unistd.h>

namespace {
using namespace omh;
using namespace std::chrono_literals;
void check(bool ok, const char* message) { if (!ok) throw std::runtime_error(message); }
template<class F> void rejects(F action) {
    try { action(); } catch (const Error&) { return; }
    throw std::runtime_error("expected rejection");
}
Json number(std::uint64_t value) { return Json::parse(std::to_string(value)); }
Json set(const Json& source, const char* name, Json value) {
    std::vector<std::pair<std::string, Json>> fields;
    for (const auto& key : source.keys()) if (key != name) fields.emplace_back(key, source.at(key));
    fields.emplace_back(name, std::move(value));
    return Json::object(fields);
}
std::map<std::string, std::string> form(std::string_view text) {
    std::map<std::string, std::string> out;
    while (!text.empty()) {
        const auto amp = text.find('&');
        const auto field = text.substr(0, amp);
        const auto equal = field.find('=');
        check(equal != std::string_view::npos, "fixture form");
        int size = 0;
        auto* decoded = curl_easy_unescape(nullptr, field.data() + equal + 1, static_cast<int>(field.size() - equal - 1), &size);
        check(decoded, "fixture decode");
        out.emplace(std::string(field.substr(0, equal)), std::string(decoded, static_cast<std::size_t>(size)));
        curl_free(decoded);
        if (amp == std::string_view::npos) break;
        text.remove_prefix(amp + 1);
    }
    return out;
}
struct Temp {
    std::filesystem::path root, path;
    Temp() {
        char directory[] = "/tmp/omh-auth-XXXXXX";
        const auto result = ::mkdtemp(directory);
        check(result, "mkdtemp");
        root = result;
        path = root / "auth";
    }
    ~Temp() { std::error_code error; std::filesystem::remove_all(root, error); }
};
class Key {
    std::unique_ptr<EVP_PKEY, decltype(&EVP_PKEY_free)> key_{nullptr, EVP_PKEY_free};
public:
    Key() {
        auto context = std::unique_ptr<EVP_PKEY_CTX, decltype(&EVP_PKEY_CTX_free)>(EVP_PKEY_CTX_new_id(EVP_PKEY_RSA, nullptr), EVP_PKEY_CTX_free);
        EVP_PKEY* key = nullptr;
        check(context && EVP_PKEY_keygen_init(context.get()) == 1 &&
              EVP_PKEY_CTX_set_rsa_keygen_bits(context.get(), 2048) == 1 &&
              EVP_PKEY_keygen(context.get(), &key) == 1, "fixture key generation");
        key_.reset(key);
    }
    Json jwks() const {
        auto encode = [&](const char* name) {
            BIGNUM* raw = nullptr;
            check(EVP_PKEY_get_bn_param(key_.get(), name, &raw) == 1, "fixture public key");
            auto value = std::unique_ptr<BIGNUM, decltype(&BN_free)>(raw, BN_free);
            std::string bytes(static_cast<std::size_t>(BN_num_bytes(value.get())), '\0');
            BN_bn2bin(value.get(), reinterpret_cast<unsigned char*>(bytes.data()));
            return Json::string(base64url(bytes));
        };
        return Json::object({{"keys", Json::array({Json::object({{"kty", Json::string("RSA")},
            {"kid", Json::string("fixture_key")}, {"alg", Json::string("RS256")}, {"use", Json::string("sig")},
            {"n", encode(OSSL_PKEY_PARAM_RSA_N)}, {"e", encode(OSSL_PKEY_PARAM_RSA_E)}})})}});
    }
    std::string sign(const Json& claims, std::string_view algorithm = "RS256") const {
        const auto header = Json::object({{"alg", Json::string(algorithm)}, {"kid", Json::string("fixture_key")}});
        const auto message = base64url(header.dump()) + "." + base64url(claims.dump());
        auto context = std::unique_ptr<EVP_MD_CTX, decltype(&EVP_MD_CTX_free)>(EVP_MD_CTX_new(), EVP_MD_CTX_free);
        check(context && EVP_DigestSignInit(context.get(), nullptr, EVP_sha256(), nullptr, key_.get()) == 1, "fixture sign");
        std::size_t length = 0;
        check(EVP_DigestSign(context.get(), nullptr, &length,
              reinterpret_cast<const unsigned char*>(message.data()), message.size()) == 1, "fixture sign");
        std::string signature(length, '\0');
        check(EVP_DigestSign(context.get(), reinterpret_cast<unsigned char*>(signature.data()), &length,
              reinterpret_cast<const unsigned char*>(message.data()), message.size()) == 1, "fixture sign");
        signature.resize(length);
        return message + "." + base64url(signature);
    }
};
Json claims(std::uint64_t now, std::string nonce) {
    return Json::object({{"iss", Json::string(auth_issuer)}, {"sub", Json::string("fixture_subject")},
        {"aud", Json::string("fixture_client")}, {"iat", number(now)}, {"exp", number(now + 3600)},
        {"nonce", Json::string(std::move(nonce))}});
}
HttpReply reply(const Json& value, long status = 200) { return {{status, {"application/json"}}, value.dump()}; }

struct Mock {
    Key key;
    std::uint64_t now = 1800000000;
    std::string nonce, challenge, redirect, host;
    std::atomic<unsigned> refreshes{0}, inference{0}, revokes{0}, grants{0};
    std::vector<std::string> issued_refresh, revoked_refresh;
    bool permission = true, revoke_failure = false, crash_refresh = false, block_refresh = false;
    std::atomic<bool> refresh_entered = false;
    int refresh_signal = -1;
    std::jthread browser;
    ~Mock() { if (browser.joinable()) browser.join(); }
    HttpReply exchange(const HttpRequest& request, std::stop_token stop) {
        if (request.url.ends_with("openid-configuration"))
            return reply(Json::parse(R"({"issuer":"https://auth.openai.com","authorization_endpoint":"https://auth.openai.com/api/accounts/authorize","token_endpoint":"https://auth.openai.com/api/accounts/oauth/token","jwks_uri":"https://auth.openai.com/.well-known/jwks.json","revocation_endpoint":"https://auth.openai.com/api/accounts/oauth/revoke"})"));
        if (request.url.ends_with("jwks.json")) return reply(key.jwks());
        if (request.url.ends_with("/token")) {
            const auto fields = form(request.body);
            check(fields.at("client_id") == "fixture_client" && fields.at("resource") == "https://api.openai.com/v1",
                  "token exchange lost client/resource");
            const bool refresh = fields.at("grant_type") == "refresh_token";
            if (refresh) {
                ++refreshes;
                if (refresh_signal >= 0) {
                    const char marker = 'R';
                    const auto sent = ::write(refresh_signal, &marker, 1);
                    check(sent == 1, "refresh signal");
                }
                if (block_refresh) {
                    refresh_entered = true;
                    std::mutex mutex;
                    std::condition_variable_any condition;
                    std::unique_lock lock(mutex);
                    condition.wait_for(lock, stop, 5s, [] { return false; });
                    require(!stop.stop_requested(), "fixture_cancelled", Stage::frontend, ErrorCode::cancelled);
                }
                if (crash_refresh) ::_exit(9);
                check(!fields.contains("scope"), "refresh changed scopes");
            } else {
                ++grants;
                check(fields.at("code") == "fixture_code" && fields.at("redirect_uri") == redirect &&
                      pkce_challenge(fields.at("code_verifier")) == challenge, "PKCE/code/redirect mismatch");
            }
            const auto renewable = "fixture_refresh_" + std::to_string(grants.load()) + "_" + std::to_string(refreshes.load());
            issued_refresh.push_back(renewable);
            return reply(Json::object({{"access_token", Json::string("fixture_access_" + std::to_string(refreshes.load()))},
                {"refresh_token", Json::string(renewable)},
                {"id_token", Json::string(key.sign(claims(now, nonce)))}, {"token_type", Json::string("Bearer")},
                {"expires_in", number(3600)}, {"earliest_refresh_at", number(now)},
                {"scope", Json::string(permission ? "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct" : "openid")}}));
        }
        if (request.url.ends_with("/revoke")) {
            ++revokes;
            revoked_refresh.push_back(form(request.body).at("token"));
            return {{revoke_failure ? 503 : 200, {}}, {}};
        }
        if (request.url.ends_with("/responses")) { ++inference; return {{500, {"application/json"}}, "{}"}; }
        if (request.url.ends_with("/models")) return reply(Json::parse(R"({"models":[]})"));
        throw std::runtime_error("unexpected fixture endpoint");
    }
    void open(std::string_view url) {
        const auto query = form(url.substr(url.find('?') + 1));
        check(query.at("code_challenge_method") == "S256", "missing S256");
        nonce = query.at("nonce");
        challenge = query.at("code_challenge");
        redirect = query.at("redirect_uri");
        if (!host.empty()) check(host == query.at("ext_agent_host_id"), "host ID changed");
        host = query.at("ext_agent_host_id");
        const auto state = query.at("state");
        if (browser.joinable()) browser.join();
        browser = std::jthread([state, address = redirect] {
            const auto port_start = address.find(':', 7) + 1;
            const auto path_start = address.find('/', port_start);
            const auto port = static_cast<std::uint16_t>(std::stoi(address.substr(port_start, path_start - port_start)));
            const auto connection = ::socket(AF_INET, SOCK_STREAM, 0);
            sockaddr_in endpoint{};
            endpoint.sin_family = AF_INET;
            endpoint.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
            endpoint.sin_port = htons(port);
            if (::connect(connection, reinterpret_cast<sockaddr*>(&endpoint), sizeof(endpoint)) != 0) ::_exit(12);
            const auto message = "GET /auth/callback?" + form_encode({{"state", state},
                {"code", "fixture_code"}, {"client_id", "fixture_client"}}) + " HTTP/1.1\r\nHost: 127.0.0.1:" +
                std::to_string(port) + "\r\n\r\n";
            const auto sent = ::send(connection, message.data(), message.size(), MSG_NOSIGNAL);
            if (sent != static_cast<ssize_t>(message.size())) ::_exit(13);
            char response[256];
            const auto received = ::recv(connection, response, sizeof(response), 0);
            if (received <= 0) ::_exit(14);
            ::close(connection);
        });
    }
    std::unique_ptr<AuthManager> manager(const std::filesystem::path& path, AuthStoreObserver observer = {}, StreamingExchange streaming = {}) {
        return std::make_unique<AuthManager>(path,
            [this](const HttpRequest& request, std::stop_token stop) { return exchange(request, stop); },
            [this] { return now; }, std::move(observer), std::move(streaming));
    }
    void login(AuthManager& manager) {
        manager.login([this](std::string_view url) { open(url); });
        browser.join();
    }
};

void jwt_validation() {
    Key key;
    constexpr std::uint64_t now = 1800000000;
    const auto good = claims(now, "nonce");
    check(verify_id_token(key.sign(good), key.jwks(), "fixture_client", "nonce", now).text("sub") == "fixture_subject", "good JWT rejected");
    for (const auto& invalid : std::vector<Json>{
        set(good, "iss", Json::string("https://attacker.invalid")), set(good, "aud", Json::string("other")),
        set(good, "nonce", Json::string("other")), set(good, "exp", number(now)),
        set(good, "nbf", number(now + 1000)), set(good, "iat", number(now + 1000))})
        rejects([&] { verify_id_token(key.sign(invalid), key.jwks(), "fixture_client", "nonce", now); });
    rejects([&] { verify_id_token(key.sign(good, "none"), key.jwks(), "fixture_client", "nonce", now); });
    auto damaged = key.sign(good);
    damaged.back() = damaged.back() == 'A' ? 'B' : 'A';
    rejects([&] { verify_id_token(damaged, key.jwks(), "fixture_client", "nonce", now); });
    const auto public_key = key.jwks().at("keys").elements().front();
    const auto duplicate = Json::object({{"keys", Json::array({public_key, public_key})}});
    rejects([&] { verify_id_token(key.sign(good), duplicate, "fixture_client", "nonce", now); });
    check(pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk") ==
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM", "RFC PKCE oracle");
}
void lifecycle() {
    Temp temp;
    Mock mock;
    auto manager = mock.manager(temp.path);
    check(!manager->status().logged_in, "new store signed in");
    mock.login(*manager);
    const auto first = manager->access();
    check(manager->status().logged_in && first.account.size() == 64, "login not persisted");
    manager.reset();
    manager = mock.manager(temp.path);
    check(manager->access().session == first.session, "restart changed session");
    mock.now += 3550;
    std::jthread one([&] { (void)manager->access(); });
    std::jthread two([&] { (void)manager->access(); });
    one.join(); two.join();
    check(mock.refreshes == 1 && manager->access().bearer != first.bearer, "rotating refresh raced");
    struct stat status{};
    check(::stat((temp.path / "credentials.json").c_str(), &status) == 0 && (status.st_mode & 077) == 0,
          "credentials exposed");
    mock.revoke_failure = true;
    check(!manager->logout() && !manager->status().logged_in && !std::filesystem::exists(temp.path / "credentials.json"),
          "logout retained credentials");
    check(!manager->logout(), "unconfirmed revocation became confirmed");
    rejects([&] { manager->access(); });
    mock.login(*manager);
    check(manager->status().logged_in && manager->access().session != first.session, "reauthorization failed");
}
void permissions() {
    Temp temp;
    Mock mock;
    mock.permission = false;
    auto manager = mock.manager(temp.path);
    mock.login(*manager);
    check(manager->status().logged_in && !manager->status().plan_enabled, "identity-only grant not preserved");
    rejects([&] { manager->access(); });
    check(mock.inference == 0, "missing grant reached inference");
}
void refresh_crash() {
    Temp temp;
    Mock mock;
    { auto manager = mock.manager(temp.path); mock.login(*manager); }
    const auto child = ::fork();
    check(child >= 0, "fork");
    if (child == 0) {
        mock.crash_refresh = true;
        auto manager = mock.manager(temp.path);
        (void)manager->access({}, true);
        ::_exit(4);
    }
    int status = 0;
    check(::waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 9, "refresh crash fixture");
    auto manager = mock.manager(temp.path);
    check(manager->status().refresh_pending && !manager->status().logged_in, "ambiguous refresh reused");
    rejects([&] { manager->access(); });
    check(mock.refreshes == 0, "parent retried uncertain rotating token");
    mock.login(*manager);
    check(manager->status().logged_in, "reauthorization after uncertain refresh failed");
    check(!manager->logout(), "reauthorization concealed unknown rotating replacement");
}
void pending_cleanup() {
    Temp temp;
    Mock mock;
    { auto manager = mock.manager(temp.path); mock.login(*manager); }
    const auto child = ::fork();
    check(child >= 0, "fork");
    if (child == 0) {
        auto manager = mock.manager(temp.path, [](AuthStorePoint point, std::string_view name) {
            if (point == AuthStorePoint::temporary_synced && name == "credentials.json") ::_exit(8);
        });
        (void)manager->access({}, true);
        ::_exit(4);
    }
    int status = 0;
    check(::waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 8, "pending crash fixture");
    auto manager = mock.manager(temp.path);
    (void)manager->status();
    for (const auto& entry : std::filesystem::directory_iterator(temp.path))
        check(!entry.path().filename().string().starts_with("pending-"), "crash credential copy retained");
    check(manager->logout(), "logout failed");
}
void dispatch_logout() {
    Temp temp;
    Mock mock;
    std::mutex mutex;
    std::condition_variable changed;
    bool waiting = false, release = false;
    auto manager = mock.manager(temp.path, [&](AuthStorePoint point, std::string_view) {
        if (point != AuthStorePoint::before_dispatch) return;
        std::unique_lock lock(mutex);
        waiting = true;
        changed.notify_all();
        check(changed.wait_for(lock, 5s, [&] { return release; }), "dispatch release");
    });
    mock.login(*manager);
    const auto lease = manager->access();
    ToolRegistry registry; registry.register_tool(test_value_tool());
    Request request({"openai-siwc", lease.account, "gpt-6.1-sol", "gpt-6.1-sol"},
        {"fixture", "fixture", Json::parse("{}")}, registry.snapshot()->advertisement(), {});
    std::atomic<bool> rejected = false;
    std::jthread worker([&] { rejected = std::holds_alternative<Error>(manager->generate(request)); });
    {
        std::unique_lock lock(mutex);
        check(changed.wait_for(lock, 5s, [&] { return waiting; }), "dispatch barrier");
    }
    check(manager->logout(), "logout");
    {
        std::lock_guard lock(mutex); release = true; changed.notify_all();
    }
    worker.join();
    check(rejected && mock.inference == 0, "request dispatched after completed logout");
}
void streaming_logout() {
    Temp temp; Mock mock;
    std::atomic<bool> delivered = false, stopped = false, rejected = false;
    auto manager = mock.manager(temp.path, {}, [&](const Json&, std::string_view, const auto& sink, std::stop_token stop) -> Result<std::monostate> {
        sink("data: {\"type\":\"response.created\",\"sequence_number\":0,\"response\":{\"id\":\"r\",\"model\":\"gpt-6.1-sol\",\"status\":\"in_progress\",\"output\":[]}}\n\n");
        sink("data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"m\",\"role\":\"assistant\",\"status\":\"in_progress\",\"content\":[]}}\n\n");
        sink("data: {\"type\":\"response.content_part.added\",\"sequence_number\":2,\"item_id\":\"m\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"output_text\",\"text\":\"\"}}\n\n");
        sink("data: {\"type\":\"response.output_text.delta\",\"sequence_number\":3,\"item_id\":\"m\",\"output_index\":0,\"content_index\":0,\"delta\":\"hi\"}\n\n");
        delivered = true;
        while (!stop.stop_requested()) std::this_thread::sleep_for(1ms);
        stopped = true;
        return std::monostate{};
    });
    mock.login(*manager);
    const auto lease = manager->access();
    Request request({"openai-siwc", lease.account, "gpt-6.1-sol", "gpt-6.1-sol"},
        {"fixture", "fixture", Json::parse("{}")}, {"empty", {}}, {});
    std::atomic<bool> preview = false;
    std::jthread worker([&] { rejected = std::holds_alternative<Error>(manager->generate(request, {}, {},
        [&](std::string_view text) { preview = text == "hi"; })); });
    const auto deadline = std::chrono::steady_clock::now() + 5s;
    while (!delivered && std::chrono::steady_clock::now() < deadline) std::this_thread::sleep_for(1ms);
    check(delivered && preview, "stream preview before completion");
    check(manager->logout(), "stream logout");
    worker.join();
    check(stopped && rejected, "logout cancels active stream");
}
void wait_flag(const std::atomic<bool>& flag) {
    const auto deadline = std::chrono::steady_clock::now() + 5s;
    while (!flag) {
        check(std::chrono::steady_clock::now() < deadline, "fixture barrier timeout");
        std::this_thread::sleep_for(1ms);
    }
}
void logout_login() {
    Temp temp;
    Mock mock;
    auto manager = mock.manager(temp.path);
    std::atomic<bool> listening = false, cancelled = false;
    std::jthread worker([&] {
        try { manager->login([&](std::string_view) { listening = true; }); }
        catch (const Error& error) { cancelled = error.code == ErrorCode::cancelled; }
    });
    wait_flag(listening);
    auto other_process_client = mock.manager(temp.path);
    check(other_process_client->logout(), "new logout client blocked by constructor");
    worker.join();
    check(cancelled && !manager->status().logged_in, "logout did not cancel waiting login");
}
void logout_refresh() {
    Temp temp;
    Mock mock;
    auto manager = mock.manager(temp.path);
    mock.login(*manager);
    mock.block_refresh = true;
    std::atomic<bool> cancelled = false;
    std::jthread worker([&] {
        try { (void)manager->access({}, true); }
        catch (const Error& error) { cancelled = error.code == ErrorCode::cancelled; }
    });
    wait_flag(mock.refresh_entered);
    auto another = mock.manager(temp.path);
    check(!another->logout(), "ambiguous refresh was reported remotely revoked");
    worker.join();
    check(cancelled && !another->status().logged_in && !std::filesystem::exists(temp.path / "credentials.json"),
          "logout retained interrupted refresh tokens");
}
void logout_crash() {
    Temp temp;
    Mock mock;
    { auto manager = mock.manager(temp.path); mock.login(*manager); }
    const auto child = ::fork();
    check(child >= 0, "fork");
    if (child == 0) {
        auto manager = mock.manager(temp.path, [](AuthStorePoint point, std::string_view) {
            if (point == AuthStorePoint::tokens_removed) ::_exit(7);
        });
        (void)manager->logout();
        ::_exit(4);
    }
    int status = 0;
    check(::waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 7, "logout crash");
    auto manager = mock.manager(temp.path);
    check(!manager->logout() && mock.revokes == 0, "revoke confirmed without request");
}
void logout_marker_recovery() {
    Temp temp;
    Mock mock;
    { auto manager = mock.manager(temp.path); mock.login(*manager); }
    const auto child = ::fork();
    check(child >= 0, "fork");
    if (child == 0) {
        auto manager = mock.manager(temp.path, [](AuthStorePoint point, std::string_view name) {
            if (point == AuthStorePoint::replaced && name == "registration.json") ::_exit(6);
        });
        (void)manager->logout();
        ::_exit(4);
    }
    int status = 0;
    check(::waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 6, "logout intent crash");
    auto manager = mock.manager(temp.path);
    rejects([&] { manager->login([](std::string_view) { fail(ErrorCode::validation, Stage::frontend, "fixture_browser_failure"); }); });
    check(!manager->status().logged_in, "failed login revived logged-out tokens");
    rejects([&] { manager->access(); });
}
void process_refresh() {
    Temp temp;
    Mock mock;
    { auto manager = mock.manager(temp.path); mock.login(*manager); }
    mock.now += 3550;
    int channel[2];
    check(::pipe(channel) == 0, "pipe");
    mock.refresh_signal = channel[1];
    std::vector<pid_t> children;
    for (unsigned index = 0; index < 2; ++index) {
        const auto child = ::fork();
        check(child >= 0, "fork");
        if (child == 0) {
            ::close(channel[0]);
            try { auto manager = mock.manager(temp.path); (void)manager->access(); }
            catch (...) { ::_exit(3); }
            ::_exit(0);
        }
        children.push_back(child);
    }
    ::close(channel[1]);
    for (const auto child : children) {
        int status = 0;
        check(::waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0,
              "process refresh failed");
    }
    char signals[8];
    const auto count = ::read(channel[0], signals, sizeof(signals));
    ::close(channel[0]);
    check(count == 1, "processes raced rotating refresh");
}
void refresh_scope_loss() {
    Temp temp;
    Mock mock;
    auto manager = mock.manager(temp.path);
    mock.login(*manager);
    mock.permission = false;
    rejects([&] { manager->access({}, true); });
    check(manager->status().logged_in && !manager->status().plan_enabled, "refresh scope loss not retained");
    rejects([&] { manager->models(); });
    check(mock.inference == 0, "scope loss allowed resource use");
}
void uncertain_login() {
    Temp temp;
    Mock mock;
    { auto manager = mock.manager(temp.path); mock.login(*manager); }
    const auto child = ::fork();
    check(child >= 0, "fork");
    if (child == 0) {
        auto manager = mock.manager(temp.path, [](AuthStorePoint point, std::string_view name) {
            if (point == AuthStorePoint::temporary_synced && name == "credentials.json") ::_exit(8);
        });
        mock.login(*manager);
        ::_exit(4);
    }
    int status = 0;
    check(::waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 8, "login exchange crash");
    auto manager = mock.manager(temp.path);
    mock.login(*manager);
    check(!manager->logout(), "unknown previous login was reported fully revoked");

    Temp active;
    Mock server;
    auto current = server.manager(active.path);
    server.login(*current);
    server.login(*current);
    check(server.issued_refresh.size() == 2 && server.issued_refresh[0] != server.issued_refresh[1],
          "reauthorization fixture did not create separate grants");
    check(!current->logout(), "revoke of replacement falsely confirmed previous grant");
    check(server.revoked_refresh.size() == 1 && server.revoked_refresh[0] == server.issued_refresh[1],
          "logout did not target current grant");
    current.reset();
    current = server.manager(active.path);
    check(!current->status().logged_in && !current->logout(), "restart lost previous grant uncertainty");
}
void committed_login_crash() {
    Temp temp;
    Mock mock;
    const auto child = ::fork();
    check(child >= 0, "fork");
    if (child == 0) {
        auto manager = mock.manager(temp.path, [](AuthStorePoint point, std::string_view name) {
            if (point == AuthStorePoint::replaced && name == "credentials.json") ::_exit(8);
        });
        mock.login(*manager);
        ::_exit(4);
    }
    int status = 0;
    check(::waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 8,
          "committed login crash fixture");
    auto manager = mock.manager(temp.path);
    check(manager->status().logged_in && !manager->access().session.empty(),
          "committed login lost identity binding after crash");
}
void held_dispatch() {
    Temp temp;
    Mock mock;
    auto manager = mock.manager(temp.path);
    mock.login(*manager);
    const auto lease = manager->access();
    auto hold = manager->hold_session(lease.session);
    std::atomic<bool> done = false;
    std::jthread logout([&] {
        auto another = mock.manager(temp.path);
        (void)another->logout();
        done = true;
    });
    const auto deadline = std::chrono::steady_clock::now() + 5s;
    while (!std::filesystem::exists(temp.path / "signed-out")) {
        check(std::chrono::steady_clock::now() < deadline, "logout marker timeout");
        std::this_thread::sleep_for(1ms);
    }
    check(!done, "logout completed before admitted executor lease released");
    hold.reset();
    logout.join();
    check(done, "logout did not finish after lease release");
    rejects([&] { (void)manager->hold_session(lease.session); });
}
void execution_logout() {
    Temp temp; Mock mock;
    auto manager = mock.manager(temp.path); mock.login(*manager);
    const auto identity = manager->access().session;
    std::stop_source parent;
    auto execution = manager->hold_execution(identity, parent.get_token());
    parent.request_stop();
    check(execution.stop.stop_requested(), "parent cancellation did not reach executor");
    execution.ownership.reset();
    execution = manager->hold_execution(identity);
    manager.reset(); // The watcher owns its descriptor, not the AuthManager.
    std::atomic<bool> done = false;
    std::jthread logout([&] {
        auto another = mock.manager(temp.path); (void)another->logout(); done = true;
    });
    const auto deadline = std::chrono::steady_clock::now() + 5s;
    while (!execution.stop.stop_requested()) {
        check(std::chrono::steady_clock::now() < deadline, "logout did not cancel active execution");
        std::this_thread::sleep_for(1ms);
    }
    check(!done, "logout bypassed execution lease");
    execution.ownership.reset(); logout.join();
    check(done, "logout blocked after stopped executor released lease");
}
}
int main(int argc, char** argv) {
    try {
        check(argc == 2, "suite required");
        const std::string_view suite = argv[1];
        if (suite == "jwt") jwt_validation();
        else if (suite == "lifecycle") lifecycle();
        else if (suite == "permission") permissions();
        else if (suite == "refresh-crash") refresh_crash();
        else if (suite == "pending-cleanup") pending_cleanup();
        else if (suite == "dispatch-logout") { dispatch_logout(); streaming_logout(); }
        else if (suite == "logout-login") logout_login();
        else if (suite == "logout-refresh") logout_refresh();
        else if (suite == "logout-crash") logout_crash();
        else if (suite == "logout-marker") logout_marker_recovery();
        else if (suite == "process-refresh") process_refresh();
        else if (suite == "refresh-scope") refresh_scope_loss();
        else if (suite == "uncertain-login") uncertain_login();
        else if (suite == "committed-login-crash") committed_login_crash();
        else if (suite == "held-dispatch") held_dispatch();
        else if (suite == "execution-logout") execution_logout();
        else throw std::runtime_error("unknown suite");
        return 0;
    } catch (const std::exception& error) { std::cerr << error.what() << '\n'; return 1; }
}
