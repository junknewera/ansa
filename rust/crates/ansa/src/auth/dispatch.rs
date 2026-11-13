//! Auth guard owned by the serialized dispatch path; retains no bearer.
use super::{
    Cancel, Result,
    manager::{AuthManager, Status},
};
use std::sync::Arc;
pub struct Authorization {
    auth: Arc<AuthManager>,
    account: String,
    session: String,
    model: String,
    epoch: u64,
    model_checked: bool,
}
impl Authorization {
    pub fn new(auth: Arc<AuthManager>, status: &Status, model: String) -> Result<Self> {
        if !status.logged_in
            || !status.plan_enabled
            || status.account.is_empty()
            || status.session.is_empty()
        {
            return Err("auth_required");
        }
        Ok(Self {
            auth,
            account: status.account.clone(),
            session: status.session.clone(),
            model,
            epoch: 0,
            model_checked: false,
        })
    }
    pub fn account(&self) -> &str {
        &self.account
    }
    pub fn session(&self) -> &str {
        &self.session
    }
    pub fn check_dispatch(&mut self, epoch: u64, cancel: &Cancel) -> Result<()> {
        let current = self.auth.access(cancel, false)?;
        if current.account != self.account {
            return Err("auth_account_mismatch");
        }
        if epoch == self.epoch && current.session != self.session {
            return Err("auth_required");
        }
        if !self.model_checked || epoch != self.epoch {
            let catalog = self.auth.models(cancel)?;
            let mut available = false;
            for model in catalog.field("models")?.elements()? {
                if model.text_field("slug")? == self.model
                    && model.text_field("visibility")? == "list"
                {
                    available = true;
                    break;
                }
            }
            if !available {
                return Err("model_not_available");
            }
            let verified = self.auth.access(cancel, false)?;
            if verified.account != self.account {
                return Err("auth_account_mismatch");
            }
            if verified.session != current.session {
                return Err("auth_required");
            }
            self.session = current.session;
            self.epoch = epoch;
            self.model_checked = true;
        }
        Ok(())
    }
}
