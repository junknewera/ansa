#pragma once
#include "omh/transport.hpp"
#include <filesystem>

namespace omh {
inline constexpr std::string_view auth_issuer = "https://auth.openai.com";
struct AuthEndpoints {
    std::string authorization, token, jwks, revocation;
    static AuthEndpoints discover(const Json&);
};
std::string random_url_token(std::size_t bytes = 32);
std::string base64url(std::string_view);
std::string unbase64url(std::string_view);
std::string pkce_challenge(std::string_view);
std::string form_encode(const std::vector<std::pair<std::string, std::string>>&);
Json auth_callback_fields(std::string_view request, std::string_view state, std::uint16_t port);
Json verify_id_token(std::string_view jwt, const Json& jwks, std::string_view client,
    std::string_view nonce, std::uint64_t now, bool refresh = false);

struct AuthLease {
    std::string bearer, account, session;
    std::uint64_t expires_at = 0;
};
struct AuthStatus {
    bool logged_in = false, refresh_pending = false, plan_enabled = false;
    std::string account;
    std::uint64_t expires_in = 0, refresh_wait = 0;
    std::string session;
};
struct AuthExecutionLease { std::shared_ptr<void> ownership; std::stop_token stop; };
enum class AuthStorePoint { temporary_synced, replaced, before_dispatch, tokens_removed, logout_marked };
using StreamingExchange = std::function<Result<std::monostate>(const Json&, std::string_view,
    const std::function<void(std::string_view)>&, std::stop_token)>;
using AuthStoreObserver = std::function<void(AuthStorePoint, std::string_view name)>;

// Own credential directory only. All read/replace/refresh operations share a
// stop-aware process lock. Tokens never enter the session journal or diagnostics.
class AuthManager {
    class Impl;
    std::unique_ptr<Impl> impl_;
public:
    explicit AuthManager(std::filesystem::path, HttpExchange = http_exchange,
                         std::function<std::uint64_t()> clock = {}, AuthStoreObserver = {}, StreamingExchange = {});
    ~AuthManager();
    void login(const std::function<void(std::string_view url)>& open_browser, std::stop_token = {}, bool consent = false);
    AuthStatus status();
    AuthLease access(std::stop_token = {}, bool force_refresh = false);
    bool logout(std::stop_token = {});
    bool current(const AuthLease&);
    std::shared_ptr<void> hold_session(std::string_view session, std::stop_token = {});
    AuthExecutionLease hold_execution(std::string_view session, std::stop_token = {});
    Json models(std::stop_token = {});
    Result<CompletedResponse> generate(const Request&, std::stop_token = {}, std::string_view expected_session = {}, TextPreview = {});
};
std::filesystem::path default_auth_directory();
} // namespace omh
