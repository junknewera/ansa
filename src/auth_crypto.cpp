#include "omh/auth.hpp"
#include <openssl/core_names.h>
#include <openssl/evp.h>
#include <openssl/param_build.h>
#include <openssl/rand.h>
#include <algorithm>
#include <array>
#include <set>

namespace omh {
namespace {
void checked(bool valid, const char* code) { require(valid, code, Stage::frontend, ErrorCode::validation); }
}
std::string base64url(std::string_view bytes) {
    checked(bytes.size() <= 1024 * 1024, "auth_encoding_limit");
    std::string text(4 * ((bytes.size() + 2) / 3), '\0');
    EVP_EncodeBlock(reinterpret_cast<unsigned char*>(text.data()),
                    reinterpret_cast<const unsigned char*>(bytes.data()), static_cast<int>(bytes.size()));
    while (text.ends_with('=')) text.pop_back();
    std::replace(text.begin(), text.end(), '+', '-');
    std::replace(text.begin(), text.end(), '/', '_');
    return text;
}
std::string unbase64url(std::string_view encoded) {
    checked(!encoded.empty() && encoded.size() <= 64 * 1024 && encoded.size() % 4 != 1 &&
        std::all_of(encoded.begin(), encoded.end(), [](unsigned char c) {
            return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') ||
                   (c >= '0' && c <= '9') || c == '-' || c == '_';
        }), "auth_encoding");
    std::string text(encoded);
    std::replace(text.begin(), text.end(), '-', '+');
    std::replace(text.begin(), text.end(), '_', '/');
    while (text.size() % 4) text += '=';
    std::string decoded(text.size() / 4 * 3, '\0');
    const int size = EVP_DecodeBlock(reinterpret_cast<unsigned char*>(decoded.data()),
        reinterpret_cast<const unsigned char*>(text.data()), static_cast<int>(text.size()));
    checked(size >= 0, "auth_encoding");
    const auto padding = text.size() - encoded.size();
    decoded.resize(static_cast<std::size_t>(size) - padding);
    checked(base64url(decoded) == encoded, "auth_encoding");
    return decoded;
}
std::string random_url_token(std::size_t bytes) {
    checked(bytes >= 16 && bytes <= 64, "auth_random_size");
    std::string value(bytes, '\0');
    checked(RAND_bytes(reinterpret_cast<unsigned char*>(value.data()), static_cast<int>(bytes)) == 1, "auth_random");
    return base64url(value);
}
std::string pkce_challenge(std::string_view verifier) {
    std::array<unsigned char, 32> digest{};
    unsigned size = 0;
    checked(EVP_Digest(verifier.data(), verifier.size(), digest.data(), &size, EVP_sha256(), nullptr) == 1 &&
            size == digest.size(), "auth_hash");
    return base64url({reinterpret_cast<const char*>(digest.data()), digest.size()});
}
std::string form_encode(const std::vector<std::pair<std::string, std::string>>& fields) {
    constexpr char hex[] = "0123456789ABCDEF";
    auto encode = [&](std::string_view text) {
        std::string value;
        for (unsigned char c : text) {
            if ((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') ||
                c == '-' || c == '_' || c == '.' || c == '~') value += static_cast<char>(c);
            else { value += '%'; value += hex[c >> 4]; value += hex[c & 15]; }
        }
        return value;
    };
    std::string form;
    for (const auto& [key, value] : fields) {
        if (!form.empty()) form += '&';
        form += encode(key) + "=" + encode(value);
    }
    return form;
}
AuthEndpoints AuthEndpoints::discover(const Json& discovery) {
    checked(discovery.text("issuer") == auth_issuer, "auth_discovery_issuer");
    AuthEndpoints endpoints{discovery.text("authorization_endpoint"), discovery.text("token_endpoint"),
                            discovery.text("jwks_uri"), discovery.text("revocation_endpoint")};
    checked(endpoints.authorization == "https://auth.openai.com/api/accounts/authorize" &&
            endpoints.token == "https://auth.openai.com/api/accounts/oauth/token" &&
            endpoints.jwks == "https://auth.openai.com/.well-known/jwks.json" &&
            endpoints.revocation == "https://auth.openai.com/api/accounts/oauth/revoke", "auth_discovery_endpoints");
    return endpoints;
}
Json verify_id_token(std::string_view jwt, const Json& jwks, std::string_view client,
    std::string_view nonce, std::uint64_t now, bool refresh) {
    checked(jwt.size() <= 32768, "auth_jwt_limit");
    const auto first = jwt.find('.'), last = jwt.rfind('.');
    checked(first != std::string_view::npos && last > first &&
            jwt.find('.', first + 1) == last, "auth_jwt_shape");
    const auto header = Json::parse(unbase64url(jwt.substr(0, first)), 4096, 8, 64);
    checked(header.text("alg") == "RS256" && !header.has("crit") && !header.has("jku") && !header.has("x5u"),
            "auth_jwt_algorithm");
    const auto kid = header.text("kid");
    checked(!kid.empty() && kid.size() <= 256, "auth_jwt_kid");
    auto keys = jwks.at("keys").elements();
    checked(keys.size() <= 64, "auth_jwks_limit");
    std::optional<Json> selected;
    for (const auto& key : keys) if (key.has("kid") && key.text("kid") == kid) {
        checked(!selected, "auth_duplicate_key");
        selected = key;
    }
    checked(selected.has_value(), "auth_unknown_key");
    const auto& key = *selected;
    checked(key.text("kty") == "RSA" && (!key.has("alg") || key.text("alg") == "RS256") &&
            (!key.has("use") || key.text("use") == "sig"), "auth_key_type");
    if (key.has("key_ops")) checked(key.at("key_ops") == Json::parse(R"(["verify"])"), "auth_key_usage");
    auto modulus = unbase64url(key.text("n")), exponent = unbase64url(key.text("e"));
    checked(modulus.size() >= 256 && modulus.size() <= 1024 && exponent.size() <= 8, "auth_key_size");
    using BigNum = std::unique_ptr<BIGNUM, decltype(&BN_free)>;
    BigNum n(BN_bin2bn(reinterpret_cast<const unsigned char*>(modulus.data()), static_cast<int>(modulus.size()), nullptr), BN_free);
    BigNum e(BN_bin2bn(reinterpret_cast<const unsigned char*>(exponent.data()), static_cast<int>(exponent.size()), nullptr), BN_free);
    checked(n && e && BN_num_bits(n.get()) >= 2048 && BN_is_odd(e.get()) &&
            BN_cmp(e.get(), BN_value_one()) > 0, "auth_key_size");
    auto builder = std::unique_ptr<OSSL_PARAM_BLD, decltype(&OSSL_PARAM_BLD_free)>(OSSL_PARAM_BLD_new(), OSSL_PARAM_BLD_free);
    checked(n && e && builder && OSSL_PARAM_BLD_push_BN(builder.get(), OSSL_PKEY_PARAM_RSA_N, n.get()) == 1 &&
            OSSL_PARAM_BLD_push_BN(builder.get(), OSSL_PKEY_PARAM_RSA_E, e.get()) == 1, "auth_key_build");
    auto params = std::unique_ptr<OSSL_PARAM, decltype(&OSSL_PARAM_free)>(OSSL_PARAM_BLD_to_param(builder.get()), OSSL_PARAM_free);
    auto context = std::unique_ptr<EVP_PKEY_CTX, decltype(&EVP_PKEY_CTX_free)>(
        EVP_PKEY_CTX_new_from_name(nullptr, "RSA", nullptr), EVP_PKEY_CTX_free);
    EVP_PKEY* raw = nullptr;
    checked(params && context && EVP_PKEY_fromdata_init(context.get()) == 1 &&
            EVP_PKEY_fromdata(context.get(), &raw, EVP_PKEY_PUBLIC_KEY, params.get()) == 1, "auth_key_build");
    auto public_key = std::unique_ptr<EVP_PKEY, decltype(&EVP_PKEY_free)>(raw, EVP_PKEY_free);
    auto verify = std::unique_ptr<EVP_MD_CTX, decltype(&EVP_MD_CTX_free)>(EVP_MD_CTX_new(), EVP_MD_CTX_free);
    const auto signature = unbase64url(jwt.substr(last + 1));
    checked(verify && EVP_DigestVerifyInit(verify.get(), nullptr, EVP_sha256(), nullptr, public_key.get()) == 1 &&
            EVP_DigestVerify(verify.get(), reinterpret_cast<const unsigned char*>(signature.data()), signature.size(),
                reinterpret_cast<const unsigned char*>(jwt.data()), last) == 1, "auth_jwt_signature");
    const auto claims = Json::parse(unbase64url(jwt.substr(first + 1, last - first - 1)), 16384, 16, 1024);
    checked(claims.text("iss") == auth_issuer && !claims.text("sub").empty() && claims.text("sub").size() <= 256,
            "auth_jwt_identity");
    const auto audience = claims.at("aud");
    if (audience.is_string()) checked(audience.text() == client, "auth_jwt_audience");
    else {
        const auto values = audience.elements();
        checked(!values.empty() && values.size() <= 8, "auth_jwt_audience");
        std::set<std::string> unique;
        for (const auto& value : values) checked(unique.insert(value.text()).second, "auth_jwt_audience");
        checked(unique.contains(std::string(client)) && (unique.size() == 1 ||
                (claims.has("azp") && claims.text("azp") == client)), "auth_jwt_audience");
    }
    if (claims.has("azp")) checked(claims.text("azp") == client, "auth_jwt_audience");
    checked(claims.index("exp") > now && claims.index("iat") <= now + 60 &&
            claims.index("exp") > claims.index("iat") &&
            (!claims.has("nbf") || claims.index("nbf") <= now + 60), "auth_jwt_time");
    checked((refresh && !claims.has("nonce")) || claims.text("nonce") == nonce, "auth_jwt_nonce");
    return claims;
}
} // namespace omh
