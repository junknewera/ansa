#include "omh/auth.hpp"
#include <openssl/crypto.h>
#include <algorithm>
#include <cerrno>
#include <charconv>
#include <chrono>
#include <condition_variable>
#include <cstdlib>
#include <ctime>
#include <fcntl.h>
#include <iomanip>
#include <map>
#include <netinet/in.h>
#include <poll.h>
#include <sstream>
#include <sys/file.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <thread>
#include <unistd.h>

namespace omh {
namespace {
constexpr std::string_view discovery_url = "https://auth.openai.com/.well-known/openid-configuration";
constexpr std::string_view resource = "https://api.openai.com/v1";
constexpr std::string_view scopes = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
using namespace std::chrono_literals;
void auth_require(bool valid, const char* code) { require(valid, code, Stage::frontend, ErrorCode::validation); }
Json number(std::uint64_t n) { return Json::parse(std::to_string(n)); }
class Fd {
    int value_;
public:
    explicit Fd(int value = -1) : value_(value) {}
    ~Fd() { if (value_ >= 0) ::close(value_); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int get() const { return value_; }
};
void sync_fd(int fd) {
    int result;
    do { result = ::fsync(fd); } while (result < 0 && errno == EINTR);
    auth_require(result == 0, "auth_store_sync");
}
void write_all(int fd, std::string_view text) {
    while (!text.empty()) {
        const auto count = ::write(fd, text.data(), text.size());
        if (count < 0 && errno == EINTR) continue;
        auth_require(count > 0, "auth_store_write");
        text.remove_prefix(static_cast<std::size_t>(count));
    }
}
void delay(std::stop_token stop, std::chrono::milliseconds time) {
    std::mutex mutex;
    std::condition_variable_any wake;
    std::unique_lock lock(mutex);
    wake.wait_for(lock, stop, time, [] { return false; });
}
class StoreLock {
    Fd fd_;
public:
    StoreLock(int directory, std::stop_token stop)
        : fd_(::openat(directory, "lock", O_RDWR | O_CREAT | O_CLOEXEC | O_NOFOLLOW, 0600)) {
        auth_require(fd_.get() >= 0, "auth_lock_open");
        struct stat status{};
        auth_require(::fstat(fd_.get(), &status) == 0 && S_ISREG(status.st_mode) &&
                     status.st_uid == ::geteuid() && (status.st_mode & 077) == 0 && status.st_nlink == 1,
                     "auth_lock_permissions");
        const auto deadline = std::chrono::steady_clock::now() + 30s;
        while (::flock(fd_.get(), LOCK_EX | LOCK_NB) != 0) {
            auth_require(errno == EWOULDBLOCK || errno == EAGAIN || errno == EINTR, "auth_lock");
            require(!stop.stop_requested(), "auth_cancelled", Stage::frontend, ErrorCode::cancelled);
            auth_require(std::chrono::steady_clock::now() < deadline, "auth_busy");
            delay(stop, 25ms);
        }
    }
};

void private_directory(const std::filesystem::path& path) {
    auth_require(path.is_absolute(), "auth_directory_path");
    std::vector<std::filesystem::path> missing;
    auto parent = path;
    while (!std::filesystem::exists(parent)) {
        missing.push_back(parent);
        parent = parent.parent_path();
        auth_require(!parent.empty(), "auth_directory_path");
    }
    for (auto item = missing.rbegin(); item != missing.rend(); ++item) {
        auth_require(::mkdir(item->c_str(), 0700) == 0 || errno == EEXIST, "auth_directory_create");
        Fd ancestor(::open(item->parent_path().c_str(), O_RDONLY | O_DIRECTORY | O_CLOEXEC));
        auth_require(ancestor.get() >= 0, "auth_directory_open");
        sync_fd(ancestor.get());
    }
}
Json json_reply(const HttpReply& reply) {
    auth_require(reply.head.status == 200 && reply.head.content_types.size() == 1, "auth_http_response");
    auto mime = reply.head.content_types.front();
    std::transform(mime.begin(), mime.end(), mime.begin(), [](unsigned char c) { return static_cast<char>(std::tolower(c)); });
    auth_require(mime == "application/json" || mime == "application/json; charset=utf-8" ||
                 mime == "application/json;charset=utf-8", "auth_http_mime");
    return Json::parse(reply.body, 2 * 1024 * 1024);
}
std::uint64_t timestamp(const Json& value) {
    if (value.is_integer()) {
        return Json::object({{"value", value}}).index("value");
    }
    auth_require(value.is_string(), "auth_refresh_time");
    const auto text = value.text();
    auth_require(text.size() >= 20 && text.ends_with('Z') &&
                 (text.size() == 20 || (text[19] == '.' &&
                  std::all_of(text.begin() + 20, text.end() - 1, [](char c) { return c >= '0' && c <= '9'; }))),
                 "auth_refresh_time");
    std::tm parsed{};
    std::istringstream input(text.substr(0, 19));
    input >> std::get_time(&parsed, "%Y-%m-%dT%H:%M:%S");
    auth_require(!input.fail(), "auth_refresh_time");
    const auto seconds = ::timegm(&parsed);
    auth_require(seconds > 0, "auth_refresh_time");
    return static_cast<std::uint64_t>(seconds);
}
std::string host_identifier() {
    auto bytes = unbase64url(random_url_token(16));
    bytes[6] = static_cast<char>((static_cast<unsigned char>(bytes[6]) & 15) | 64);
    bytes[8] = static_cast<char>((static_cast<unsigned char>(bytes[8]) & 63) | 128);
    constexpr char hex[] = "0123456789abcdef";
    std::string text = "urn:uuid:";
    for (std::size_t index = 0; index < bytes.size(); ++index) {
        if (index == 4 || index == 6 || index == 8 || index == 10) text += '-';
        const auto byte = static_cast<unsigned char>(bytes[index]);
        text += hex[byte >> 4];
        text += hex[byte & 15];
    }
    return text;
}
std::string account_id(const Json& record) {
    return sha256(Json::array({Json::string(auth_issuer), record.at("client_id"), record.at("subject")}).dump());
}
bool plan_enabled(std::string_view scope) {
    std::istringstream input{std::string(scope)};
    std::set<std::string> values;
    for (std::string item; input >> item;) values.insert(item);
    return values.contains("offline_access") && values.contains("resource.invoke") &&
        values.contains("chatgpt.tokens.use.direct");
}
std::string decode_form(std::string_view encoded) {
    std::string text;
    for (std::size_t i = 0; i < encoded.size(); ++i) {
        if (encoded[i] == '+') text += ' ';
        else if (encoded[i] == '%') {
            auth_require(i + 2 < encoded.size(), "auth_callback_encoding");
            unsigned c = 0;
            const auto result = std::from_chars(encoded.data() + i + 1, encoded.data() + i + 3, c, 16);
            auth_require(result.ec == std::errc{} && result.ptr == encoded.data() + i + 3 && c >= 0x20 && c != 0x7f,
                         "auth_callback_encoding");
            text += static_cast<char>(c);
            i += 2;
        } else {
            auth_require(static_cast<unsigned char>(encoded[i]) >= 0x20, "auth_callback_encoding");
            text += encoded[i];
        }
    }
    return text;
}
std::map<std::string, std::string> query_fields(std::string_view query) {
    std::map<std::string, std::string> fields;
    while (!query.empty()) {
        const auto ampersand = query.find('&');
        const auto field = query.substr(0, ampersand);
        const auto equal = field.find('=');
        auth_require(equal != std::string_view::npos && fields.size() < 16, "auth_callback_fields");
        auth_require(fields.emplace(decode_form(field.substr(0, equal)), decode_form(field.substr(equal + 1))).second,
                     "auth_callback_duplicate");
        if (ampersand == std::string_view::npos) break;
        query.remove_prefix(ampersand + 1);
    }
    return fields;
}
void browser_reply(int fd, bool success) {
    const std::string body = success ? "Sign-in received. You can return to oh-my-harness." : "Sign-in request rejected.";
    const auto response = std::string(success ? "HTTP/1.1 200 OK\r\n" : "HTTP/1.1 400 Bad Request\r\n") +
        "Content-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\n"
        "Content-Security-Policy: default-src 'none'\r\nConnection: close\r\nContent-Length: " +
        std::to_string(body.size()) + "\r\n\r\n" + body;
    const auto sent = ::send(fd, response.data(), response.size(), MSG_NOSIGNAL);
    (void)sent;
}
std::map<std::string, std::string> callback(int listener, std::string_view state, std::uint16_t port, std::stop_token stop) {
    const auto deadline = std::chrono::steady_clock::now() + 5min;
    for (unsigned attempts = 0; attempts < 64;) {
        require(!stop.stop_requested(), "auth_cancelled", Stage::frontend, ErrorCode::cancelled);
        auth_require(std::chrono::steady_clock::now() < deadline, "auth_callback_timeout");
        pollfd waiting{listener, POLLIN, 0};
        const auto ready = ::poll(&waiting, 1, 100);
        if (ready < 0 && errno == EINTR) continue;
        auth_require(ready >= 0, "auth_callback_poll");
        if (!ready) continue;
        sockaddr_in peer{};
        socklen_t peer_size = sizeof(peer);
        Fd connection(::accept4(listener, reinterpret_cast<sockaddr*>(&peer), &peer_size, SOCK_CLOEXEC | SOCK_NONBLOCK));
        if (connection.get() < 0 && (errno == EAGAIN || errno == EINTR)) continue;
        auth_require(connection.get() >= 0 && ntohl(peer.sin_addr.s_addr) == INADDR_LOOPBACK, "auth_callback_peer");
        ++attempts;
        try {
            std::string request;
            const auto read_deadline = std::chrono::steady_clock::now() + 2s;
            while (request.find("\r\n\r\n") == std::string::npos) {
                require(!stop.stop_requested(), "auth_cancelled", Stage::frontend, ErrorCode::cancelled);
                auth_require(std::chrono::steady_clock::now() < read_deadline && request.size() < 16384, "auth_callback_limit");
                pollfd readable{connection.get(), POLLIN, 0};
                if (::poll(&readable, 1, 100) <= 0) continue;
                char buffer[2048];
                const auto count = ::recv(connection.get(), buffer, sizeof(buffer), 0);
                if (count < 0 && (errno == EAGAIN || errno == EINTR)) continue;
                auth_require(count > 0, "auth_callback_read");
                request.append(buffer, static_cast<std::size_t>(count));
            }
            const auto end = request.find("\r\n");
            const auto line = std::string_view(request).substr(0, end);
            auth_require(line.starts_with("GET /auth/callback?") && line.ends_with(" HTTP/1.1") &&
                         request.find("\r\nHost: 127.0.0.1:" + std::to_string(port) + "\r\n") != std::string::npos,
                         "auth_callback_path");
            auto fields = query_fields(line.substr(19, line.size() - 19 - 9));
            const auto returned = fields.find("state");
            auth_require(returned != fields.end() && returned->second.size() == state.size() &&
                         CRYPTO_memcmp(returned->second.data(), state.data(), state.size()) == 0, "auth_callback_state");
            if (fields.contains("error")) {
                browser_reply(connection.get(), false);
                fail(ErrorCode::validation, Stage::frontend, "auth_access_denied");
            }
            auth_require(fields.contains("code") && !fields["code"].empty() && fields["code"].size() <= 8192,
                         "auth_callback_code");
            browser_reply(connection.get(), true);
            return fields;
        } catch (const Error& error) {
            browser_reply(connection.get(), false);
            if (error.diagnostic == "auth_access_denied" || error.code == ErrorCode::cancelled) throw;
        }
    }
    fail(ErrorCode::limit, Stage::frontend, "auth_callback_limit");
}
} // namespace

class AuthManager::Impl {
    const std::filesystem::path path_;
    const HttpExchange exchange_;
    const StreamingExchange streaming_;
    const std::function<std::uint64_t()> clock_;
    const AuthStoreObserver observe_;
    Fd directory_;

    std::optional<Json> read(const char* name) const {
        Fd file(::openat(directory_.get(), name, O_RDONLY | O_CLOEXEC | O_NOFOLLOW));
        if (file.get() < 0 && errno == ENOENT) return {};
        struct stat status{};
        auth_require(file.get() >= 0 && ::fstat(file.get(), &status) == 0 && S_ISREG(status.st_mode) &&
                     status.st_uid == ::geteuid() && (status.st_mode & 077) == 0 && status.st_nlink == 1 &&
                     status.st_size > 0 && status.st_size <= 256 * 1024, "auth_store_permissions");
        std::string data(static_cast<std::size_t>(status.st_size), '\0');
        std::size_t used = 0;
        while (used < data.size()) {
            const auto count = ::read(file.get(), data.data() + used, data.size() - used);
            if (count < 0 && errno == EINTR) continue;
            auth_require(count > 0, "auth_store_read");
            used += static_cast<std::size_t>(count);
        }
        return Json::parse(data, 256 * 1024);
    }
    void save(const char* name, const Json& value) const {
        const auto temporary = "pending-" + random_url_token(16);
        Fd file(::openat(directory_.get(), temporary.c_str(), O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW, 0600));
        auth_require(file.get() >= 0, "auth_store_create");
        try {
            const auto bytes = value.dump();
            auth_require(bytes.size() <= 256 * 1024, "auth_store_limit");
            write_all(file.get(), bytes);
            sync_fd(file.get());
            if (observe_) observe_(AuthStorePoint::temporary_synced, name);
            auth_require(::renameat(directory_.get(), temporary.c_str(), directory_.get(), name) == 0, "auth_store_replace");
            sync_fd(directory_.get());
            if (observe_) observe_(AuthStorePoint::replaced, name);
        } catch (...) { ::unlinkat(directory_.get(), temporary.c_str(), 0); throw; }
    }
    void remove_tokens() const {
        auth_require(::unlinkat(directory_.get(), "credentials.json", 0) == 0 || errno == ENOENT, "auth_store_remove");
        sync_fd(directory_.get());
    }
    void clean_pending() const {
        for (const auto& entry : std::filesystem::directory_iterator(path_)) {
            const auto name = entry.path().filename().string();
            if (!name.starts_with("pending-")) continue;
            struct stat status{};
            auth_require(::fstatat(directory_.get(), name.c_str(), &status, AT_SYMLINK_NOFOLLOW) == 0 &&
                         S_ISREG(status.st_mode) && status.st_uid == ::geteuid() && status.st_nlink == 1 &&
                         (status.st_mode & 077) == 0, "auth_pending_permissions");
            auth_require(::unlinkat(directory_.get(), name.c_str(), 0) == 0, "auth_store_remove");
        }
        sync_fd(directory_.get());
    }
    bool logged_out() const {
        struct stat status{};
        if (::fstatat(directory_.get(), "signed-out", &status, AT_SYMLINK_NOFOLLOW) != 0) {
            auth_require(errno == ENOENT, "auth_logout_state");
            return false;
        }
        auth_require(S_ISREG(status.st_mode) && status.st_uid == ::geteuid() &&
                     (status.st_mode & 077) == 0 && status.st_nlink == 1, "auth_logout_state");
        return true;
    }
    void request_logout() const {
        Fd marker(::openat(directory_.get(), "signed-out", O_WRONLY | O_CREAT | O_CLOEXEC | O_NOFOLLOW, 0600));
        auth_require(marker.get() >= 0 && logged_out(), "auth_logout_state");
        sync_fd(marker.get());
        sync_fd(directory_.get());
    }
    std::jthread watch_logout(std::stop_source& operation) const {
        return std::jthread([this, &operation](std::stop_token stop) {
            while (!stop.stop_requested()) {
                try { if (logged_out()) { operation.request_stop(); return; } }
                catch (...) { operation.request_stop(); return; }
                delay(stop, 50ms);
            }
        });
    }
    HttpReply authorized(HttpRequest request, const AuthLease& lease, std::stop_token stop) {
        if (observe_) observe_(AuthStorePoint::before_dispatch, "request");
        StoreLock lock(directory_.get(), stop);
        const auto record = read("credentials.json");
        auth_require(record && !logged_out() && record->at("refresh_pending") == Json::parse("false") &&
                     plan_enabled(record->text("scope")) &&
                     account_id(*record) == lease.account && record->text("session") == lease.session,
                     "auth_required");
        std::stop_source operation;
        std::stop_callback bridge(stop, [&] { operation.request_stop(); });
        auto watcher = watch_logout(operation);
        request.bearer = record->text("access_token");
        auto reply = exchange_(request, operation.get_token());
        auth_require(!logged_out(), "auth_required");
        return reply;
    }
    AuthEndpoints endpoints(std::stop_token stop) const {
        return AuthEndpoints::discover(json_reply(exchange_({"GET", std::string(discovery_url), {}, {}, {}}, stop)));
    }
    Json jwks(const AuthEndpoints& endpoints, std::stop_token stop) const {
        return json_reply(exchange_({"GET", endpoints.jwks, {}, {}, {}}, stop));
    }
    Json registration() const {
        const auto record = read("registration.json");
        auth_require(record.has_value() && record->index("version") == 1 && !record->text("host_id").empty(),
                     "auth_registration");
        return *record;
    }
    void revocation_status(bool confirmed) const {
        const auto saved = registration();
        std::vector<std::pair<std::string, Json>> fields;
        for (const auto& key : saved.keys())
            if (key != "last_revocation_confirmed") fields.emplace_back(key, saved.at(key));
        fields.emplace_back("last_revocation_confirmed", Json::parse(confirmed ? "true" : "false"));
        save("registration.json", Json::object(fields));
    }
    Json validated_tokens(const Json& response, const Json& registration, const std::string& client,
                          const std::string& nonce, const AuthEndpoints& endpoints, bool refresh,
                          std::string session, std::stop_token stop) const {
        const auto granted = response.text("scope");
        std::set<std::string> permissions;
        std::istringstream stream(granted);
        for (std::string permission; stream >> permission;) permissions.insert(permission);
        auth_require(permissions.contains("openid"), "auth_identity_permission");
        const auto now = clock_();
        const auto expiry = response.index("expires_in");
        auth_require(response.text("token_type") == "Bearer" && expiry > 0 && expiry <= 86400 &&
                     !response.text("access_token").empty() && response.text("access_token").size() <= 16384 &&
                     !response.text("refresh_token").empty() && response.text("refresh_token").size() <= 16384,
                     "auth_token_response");
        const auto verify = [&] { return verify_id_token(response.text("id_token"), jwks(endpoints, stop), client, nonce, now, refresh); };
        Json claims = [&] {
            try { return verify(); }
            catch (const Error& error) {
                if (error.diagnostic != "auth_unknown_key") throw;
                return verify();
            }
        }();
        if (registration.has("subject"))
            auth_require(claims.text("sub") == registration.text("subject"), "auth_account_mismatch");
        const auto earliest = response.has("earliest_refresh_at") ? timestamp(response.at("earliest_refresh_at")) : now;
        return Json::object({{"version", number(1)}, {"client_id", Json::string(client)},
            {"host_id", registration.at("host_id")}, {"subject", claims.at("sub")},
            {"nonce", Json::string(nonce)}, {"session", Json::string(std::move(session))},
            {"id_token", response.at("id_token")}, {"access_token", response.at("access_token")},
            {"refresh_token", response.at("refresh_token")}, {"scope", response.at("scope")},
            {"expires_at", number(now + expiry)}, {"earliest_refresh_at", number(earliest)},
            {"refresh_pending", Json::parse("false")}});
    }
    Json pending_refresh(const Json& record) const {
        std::vector<std::pair<std::string, Json>> fields;
        for (const auto& name : record.keys())
            fields.emplace_back(name, name == "refresh_pending" ? Json::parse("true") : record.at(name));
        return Json::object(fields);
    }

public:
    Impl(std::filesystem::path path, HttpExchange exchange, std::function<std::uint64_t()> clock, AuthStoreObserver observe, StreamingExchange streaming)
        : path_(std::filesystem::absolute(std::move(path))), exchange_(std::move(exchange)),
          streaming_(streaming ? std::move(streaming) : (exchange_.target<HttpReply(*)(const HttpRequest&, std::stop_token)>() &&
              *exchange_.target<HttpReply(*)(const HttpRequest&, std::stop_token)>() == &http_exchange
              ? StreamingExchange([](const Json& request, std::string_view bearer, const auto& sink, std::stop_token stop) {
                    return CurlTransport{}.post(request, bearer, sink, stop, MimePolicy::allow_missing_siwc_responses);
                }) : StreamingExchange{})),
          clock_(clock ? std::move(clock) : [] { return static_cast<std::uint64_t>(std::time(nullptr)); }),
          observe_(std::move(observe)),
          directory_((private_directory(path_), ::open(path_.c_str(), O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW))) {
        struct stat status{};
        auth_require(exchange_ && directory_.get() >= 0 && ::fstat(directory_.get(), &status) == 0 &&
                     status.st_uid == ::geteuid() && (status.st_mode & 077) == 0, "auth_directory_permissions");
    }

    void login(const std::function<void(std::string_view)>& open_browser, std::stop_token stop, bool consent) {
        StoreLock lock(directory_.get(), stop);
        clean_pending();
        const auto previous_tokens = read("credentials.json");
        if (!read("registration.json"))
            save("registration.json", Json::object({{"version", number(1)},
                {"host_id", Json::string(host_identifier())}}));
        if (logged_out() && read("credentials.json")) {
            revocation_status(false);
            remove_tokens();
        }
        auth_require(::unlinkat(directory_.get(), "signed-out", 0) == 0 || errno == ENOENT, "auth_logout_state");
        sync_fd(directory_.get());
        std::stop_source operation;
        std::stop_callback bridge(stop, [&] { operation.request_stop(); });
        auto watcher = watch_logout(operation);
        stop = operation.get_token();
        const auto saved = registration();
        const auto endpoints = this->endpoints(stop);
        const auto state = random_url_token(), nonce = random_url_token(), verifier = random_url_token();
        Fd listener(::socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0));
        auth_require(listener.get() >= 0, "auth_listener");
        sockaddr_in address{};
        address.sin_family = AF_INET;
        address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        auth_require(::bind(listener.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)) == 0 &&
                     ::listen(listener.get(), 4) == 0, "auth_listener");
        socklen_t address_size = sizeof(address);
        auth_require(::getsockname(listener.get(), reinterpret_cast<sockaddr*>(&address), &address_size) == 0, "auth_listener");
        const auto port = ntohs(address.sin_port);
        const auto redirect = "http://127.0.0.1:" + std::to_string(port) + "/auth/callback";
        std::string client = saved.has("client_id") ? saved.text("client_id") : "dynamic_agent_client";
        std::vector<std::pair<std::string, std::string>> fields{
            {"client_id", client}, {"ext_agent_host_id", saved.text("host_id")}, {"response_type", "code"},
            {"redirect_uri", redirect}, {"scope", std::string(scopes)}, {"resource", std::string(resource)},
            {"state", state}, {"nonce", nonce}, {"code_challenge_method", "S256"}, {"code_challenge", pkce_challenge(verifier)}};
        if (client == "dynamic_agent_client") fields.emplace_back("agent_name_hint", "ansa");
        if (consent) fields.emplace_back("prompt", "consent");
        open_browser(endpoints.authorization + "?" + form_encode(fields));
        const auto returned = callback(listener.get(), state, port, stop);
        if (client == "dynamic_agent_client") {
            auth_require(returned.contains("client_id") && !returned.at("client_id").empty(), "auth_registration_incomplete");
            client = returned.at("client_id");
        } else if (returned.contains("client_id"))
            auth_require(returned.at("client_id") == client, "auth_client_mismatch");
        auth_require(client != "dynamic_agent_client" && client.size() <= 256, "auth_client_id");
        // Retain the issued registration on code-exchange failure for reauthorization.
        std::vector<std::pair<std::string, Json>> registration_fields{{"version", number(1)},
            {"host_id", saved.at("host_id")}, {"client_id", Json::string(client)}};
        // Reauthorization does not promise to revoke an older renewable session.
        // Replacing its token must not later claim that all sessions were revoked.
        const bool uncertain = previous_tokens.has_value() ||
            (saved.has("last_revocation_confirmed") && saved.at("last_revocation_confirmed") == Json::parse("false")) ||
            (saved.has("exchange_pending") && saved.at("exchange_pending") == Json::parse("true")) ||
            (saved.has("revocation_uncertain") && saved.at("revocation_uncertain") == Json::parse("true"));
        registration_fields.emplace_back("revocation_uncertain", Json::parse(uncertain ? "true" : "false"));
        registration_fields.emplace_back("exchange_pending", Json::parse("true"));
        if (saved.has("subject")) registration_fields.emplace_back("subject", saved.at("subject"));
        save("registration.json", Json::object(registration_fields));
        auto reply = exchange_({"POST", endpoints.token, form_encode({
            {"grant_type", "authorization_code"}, {"client_id", client}, {"code", returned.at("code")},
            {"code_verifier", verifier}, {"redirect_uri", redirect}, {"resource", std::string(resource)}}),
            "application/x-www-form-urlencoded", {}}, stop);
        auto record = validated_tokens(json_reply(reply), saved, client, nonce, endpoints, false, random_url_token(), stop);
        // Pin the validated identity before publishing credentials. A crash
        // after that publication must leave a usable, consistently bound login.
        if (!saved.has("subject")) registration_fields.emplace_back("subject", record.at("subject"));
        save("registration.json", Json::object(registration_fields));
        save("credentials.json", record);
        save("registration.json", Json::object({{"version", number(1)}, {"host_id", saved.at("host_id")},
            {"client_id", Json::string(client)}, {"subject", record.at("subject")},
            {"revocation_uncertain", Json::parse(uncertain ? "true" : "false")},
            {"exchange_pending", Json::parse("false")}}));
    }

    AuthLease access(std::stop_token stop, bool force) {
        StoreLock lock(directory_.get(), stop);
        clean_pending();
        auth_require(!logged_out(), "auth_required");
        std::stop_source operation;
        std::stop_callback bridge(stop, [&] { operation.request_stop(); });
        auto watcher = watch_logout(operation);
        stop = operation.get_token();
        auto record = read("credentials.json");
        auth_require(record && record->index("version") == 1 &&
                     record->at("refresh_pending") == Json::parse("false"), "auth_required");
        auth_require(plan_enabled(record->text("scope")), "auth_plan_permission");
        const auto saved = registration();
        auth_require(record->text("client_id") == saved.text("client_id") &&
                     record->text("subject") == saved.text("subject") &&
                     record->text("host_id") == saved.text("host_id"), "auth_account_mismatch");
        const auto now = clock_();
        if (force || record->index("expires_at") <= now + 60) {
            auth_require(now >= record->index("earliest_refresh_at"), "auth_refresh_not_ready");
            const auto endpoints = this->endpoints(stop);
            save("credentials.json", pending_refresh(*record));
            auto reply = exchange_({"POST", endpoints.token, form_encode({{"grant_type", "refresh_token"},
                {"client_id", record->text("client_id")}, {"refresh_token", record->text("refresh_token")},
                {"resource", std::string(resource)}}), "application/x-www-form-urlencoded", {}}, stop);
            auto replacement = validated_tokens(json_reply(reply), saved, saved.text("client_id"),
                record->text("nonce"), endpoints, true, record->text("session"), stop);
            save("credentials.json", replacement);
            record = std::move(replacement);
        }
        auth_require(plan_enabled(record->text("scope")), "auth_plan_permission");
        return {record->text("access_token"), account_id(*record), record->text("session"), record->index("expires_at")};
    }
    AuthStatus status() {
        StoreLock lock(directory_.get(), {});
        clean_pending();
        const auto record = read("credentials.json");
        if (!record) return {};
        const auto now = clock_(), expiry = record->index("expires_at"), earliest = record->index("earliest_refresh_at");
        const bool pending = record->at("refresh_pending") == Json::parse("true");
        return {!pending && !logged_out(), pending, plan_enabled(record->text("scope")), account_id(*record),
            expiry > now ? expiry - now : 0, earliest > now ? earliest - now : 0};
    }
    bool current(const AuthLease& lease) {
        StoreLock lock(directory_.get(), {});
        const auto record = read("credentials.json");
        return record && !logged_out() && record->at("refresh_pending") == Json::parse("false") &&
            account_id(*record) == lease.account && record->text("session") == lease.session;
    }
    std::shared_ptr<void> hold_session(std::string_view session, std::stop_token stop) {
        auto hold = std::make_shared<StoreLock>(directory_.get(), stop);
        const auto record = read("credentials.json");
        auth_require(record && !logged_out() && record->text("session") == session &&
                     record->at("refresh_pending") == Json::parse("false") &&
                     record->index("expires_at") > clock_() && plan_enabled(record->text("scope")), "auth_required");
        return hold;
    }
    AuthExecutionLease hold_execution(std::string_view session, std::stop_token stop) {
        struct ExecutionHold {
            std::shared_ptr<void> lock;
            std::stop_source operation;
            std::optional<std::stop_callback<std::function<void()>>> bridge;
            std::jthread watcher;
        };
        auto hold = std::make_shared<ExecutionHold>();
        hold->lock = hold_session(session, stop);
        hold->bridge.emplace(stop, [source = hold->operation]() mutable { source.request_stop(); });
        auto directory = std::make_shared<Fd>(::fcntl(directory_.get(), F_DUPFD_CLOEXEC, 3));
        auth_require(directory->get() >= 0, "auth_directory_duplicate");
        hold->watcher = std::jthread([directory, source = hold->operation](std::stop_token done) mutable {
            while (!done.stop_requested()) {
                struct stat marker{};
                if (::fstatat(directory->get(), "signed-out", &marker, AT_SYMLINK_NOFOLLOW) == 0 || errno != ENOENT) {
                    source.request_stop(); return;
                }
                delay(done, 50ms);
            }
        });
        return {hold, hold->operation.get_token()};
    }
    bool logout(std::stop_token stop) {
        request_logout();
        StoreLock lock(directory_.get(), stop);
        clean_pending();
        const auto record = read("credentials.json");
        if (!record) {
            const auto saved = read("registration.json");
            return !saved || (!saved->has("client_id")) || (saved->has("last_revocation_confirmed") &&
                saved->at("last_revocation_confirmed") == Json::parse("true"));
        }
        revocation_status(false);
        remove_tokens();
        if (observe_) observe_(AuthStorePoint::tokens_removed, "credentials.json");
        bool confirmed = false;
        try {
            const auto endpoints = this->endpoints(stop);
            const auto reply = exchange_({"POST", endpoints.revocation, form_encode({{"token", record->text("refresh_token")},
                {"token_type_hint", "refresh_token"}, {"client_id", record->text("client_id")}}),
                "application/x-www-form-urlencoded", {}}, stop);
            confirmed = reply.head.status == 200 && reply.body.empty() &&
                record->at("refresh_pending") == Json::parse("false");
            const auto saved = registration();
            if ((saved.has("exchange_pending") && saved.at("exchange_pending") == Json::parse("true")) ||
                (saved.has("revocation_uncertain") && saved.at("revocation_uncertain") == Json::parse("true")))
                confirmed = false;
        } catch (...) {}
        revocation_status(confirmed);
        return confirmed;
    }
    Json models(std::stop_token stop) {
        const auto lease = access(stop, false);
        auto reply = authorized({"GET", "https://api.openai.com/v1/models", {}, {}, {}}, lease, stop);
        auto catalog = json_reply(reply);
        return catalog;
    }
    Result<CompletedResponse> generate(const Request& request, std::stop_token stop, std::string_view expected_session, TextPreview preview) {
        return capture([&] {
            const auto lease = access(stop, false);
            auth_require(request.binding().account == lease.account, "auth_account_mismatch");
            auth_require(expected_session.empty() || lease.session == expected_session, "auth_required");
            if (streaming_) {
                if (observe_) observe_(AuthStorePoint::before_dispatch, "request");
                StoreLock lock(directory_.get(), stop);
                const auto record = read("credentials.json");
                auth_require(record && !logged_out() && record->at("refresh_pending") == Json::parse("false") &&
                             plan_enabled(record->text("scope")) && account_id(*record) == lease.account &&
                             record->text("session") == lease.session, "auth_required");
                std::stop_source operation;
                std::stop_callback bridge(stop, [&] { operation.request_stop(); });
                auto watcher = watch_logout(operation);
                ResponseStream stream(request, {}, operation.get_token(), std::move(preview));
                auto transfer = streaming_(request.json(), record->text("access_token"),
                    [&](std::string_view bytes) { stream.feed(bytes); }, operation.get_token());
                if (const auto* error = std::get_if<Error>(&transfer)) throw *error;
                auth_require(!logged_out(), "auth_required");
                auto result = stream.finish();
                if (const auto* error = std::get_if<Error>(&result)) throw *error;
                return std::get<CompletedResponse>(std::move(result));
            }
            auto reply = authorized({"POST", std::string(responses_endpoint), request.json().dump(),
                "application/json", {}, 16 * 1024 * 1024, 75000}, lease, stop);
            if (reply.head.status != 200) {
                if (reply.head.status == 401) fail(ErrorCode::transport, Stage::http, "auth_required");
                if (reply.head.status == 403) fail(ErrorCode::transport, Stage::http, "provider_policy_denied");
                if (reply.head.status == 429) fail(ErrorCode::transport, Stage::http, "provider_usage_limit");
                fail(ErrorCode::transport, Stage::http, "provider_http_failure");
            }
            auto result = decode_offline(request, reply.head, {reply.body}, {}, MimePolicy::allow_missing_siwc_responses, stop, std::move(preview));
            if (auto* error = std::get_if<Error>(&result)) throw *error;
            return std::get<CompletedResponse>(std::move(result));
        });
    }
};

AuthManager::AuthManager(std::filesystem::path path, HttpExchange exchange, std::function<std::uint64_t()> clock,
    AuthStoreObserver observe, StreamingExchange streaming)
    : impl_(std::make_unique<Impl>(std::move(path), std::move(exchange), std::move(clock), std::move(observe), std::move(streaming))) {}
AuthManager::~AuthManager() = default;
void AuthManager::login(const std::function<void(std::string_view)>& browser, std::stop_token stop, bool consent) {
    impl_->login(browser, stop, consent);
}
AuthStatus AuthManager::status() { return impl_->status(); }
AuthLease AuthManager::access(std::stop_token stop, bool refresh) { return impl_->access(stop, refresh); }
bool AuthManager::logout(std::stop_token stop) { return impl_->logout(stop); }
bool AuthManager::current(const AuthLease& lease) { return impl_->current(lease); }
std::shared_ptr<void> AuthManager::hold_session(std::string_view session, std::stop_token stop) {
    return impl_->hold_session(session, stop);
}
AuthExecutionLease AuthManager::hold_execution(std::string_view session, std::stop_token stop) {
    return impl_->hold_execution(session, stop);
}
Json AuthManager::models(std::stop_token stop) { return impl_->models(stop); }
Result<CompletedResponse> AuthManager::generate(const Request& request, std::stop_token stop, std::string_view expected, TextPreview preview) {
    return impl_->generate(request, stop, expected, std::move(preview));
}
std::filesystem::path default_auth_directory() {
    const auto* home = std::getenv("HOME");
    auth_require(home && *home, "auth_home_directory");
    const auto* state = std::getenv("XDG_STATE_HOME");
    const auto legacy = state && *state ? std::filesystem::path(state) / "oh-my-harness" :
        std::filesystem::path(home) / ".local/state/oh-my-harness";
    // Existing credentials and registration stay in place during the rename.
    if (std::filesystem::exists(legacy)) return legacy;
    return std::filesystem::path(home) / ".ansa/auth";
}
} // namespace omh
