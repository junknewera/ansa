#pragma once

#include "omh/auth.hpp"
#include <algorithm>

namespace omh {
// Owned by the serialized runtime dispatch path. No bearer is retained here.
class AgentAuthorization {
    std::shared_ptr<AuthManager> auth_;
    std::string account_, session_, model_;
    std::uint64_t epoch_ = 0;
    bool model_checked_ = false;
public:
    AgentAuthorization(std::shared_ptr<AuthManager> auth, const AuthStatus& status, std::string model)
        : auth_(std::move(auth)), account_(status.account), session_(status.session), model_(std::move(model)) {
        require(auth_ && status.logged_in && status.plan_enabled && !account_.empty() && !session_.empty(),
                "auth_required", Stage::frontend);
    }
    const std::string& account() const { return account_; }
    const std::string& session() const { return session_; }
    void check_dispatch(std::uint64_t epoch, std::stop_token stop) {
        const auto current = auth_->access(stop);
        require(current.account == account_, "auth_account_mismatch", Stage::frontend);
        require(epoch != epoch_ || current.session == session_, "auth_required", Stage::frontend);
        if (!model_checked_ || epoch != epoch_) {
            const auto models = auth_->models(stop).at("models").elements();
            require(std::any_of(models.begin(), models.end(), [&](const Json& model) {
                return model.text("slug") == model_ && model.text("visibility") == "list";
            }), "model_not_available", Stage::frontend);
            const auto verified = auth_->access(stop);
            require(verified.account == account_, "auth_account_mismatch", Stage::frontend);
            require(verified.session == current.session, "auth_required", Stage::frontend);
            session_ = current.session;
            epoch_ = epoch;
            model_checked_ = true;
        }
    }
};
} // namespace omh
