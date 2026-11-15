//! Auth guard owned by the serialized dispatch path; retains no bearer.
use super::{
    Cancel, Result,
    manager::{AuthManager, Status},
};
use crate::{
    protocol::json::Json,
    provider::{contracts::GenerationSelection, request::Request},
};
use std::sync::Arc;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelCapability {
    pub model: String,
    pub display_name: String,
    pub efforts: Vec<String>,
}
/// Only explicit catalog metadata authorizes effort values. Slugs and the
/// illustrative UI levels are never a capability oracle.
pub fn model_capabilities(catalog: &Json) -> Result<Vec<ModelCapability>> {
    catalog_models(catalog, None)
}
fn catalog_models(catalog: &Json, selected: Option<&str>) -> Result<Vec<ModelCapability>> {
    let mut models = Vec::new();
    let mut names = std::collections::BTreeSet::new();
    for model in catalog.field("models")?.elements()? {
        // Authorization preserves the legacy selected-model short circuit.
        // Unrelated catalog entries cannot invalidate an admitted selection.
        if let Some(selected) = selected
            && model.text_field("slug")? != selected
        {
            continue;
        }
        if model.text_field("visibility")? != "list" {
            continue;
        }
        let slug = model.text_field("slug")?;
        if slug.is_empty()
            || slug.len() > 256
            || slug.chars().any(char::is_control)
            || !names.insert(slug.to_owned())
        {
            return Err("model_catalog");
        }
        let mut efforts = Vec::new();
        if model.has("supported_reasoning_levels") {
            for level in model.field("supported_reasoning_levels")?.elements()? {
                let effort = if level.is_string() {
                    level.text()?
                } else {
                    level.text_field("effort")?
                };
                if !crate::provider::contracts::valid_id(effort)
                    || efforts.iter().any(|value| value == effort)
                {
                    return Err("model_catalog");
                }
                efforts.push(effort.into());
            }
        }
        models.push(ModelCapability {
            model: slug.into(),
            display_name: if model.has("display_name") {
                model.text_field("display_name")?.into()
            } else {
                slug.into()
            },
            efforts,
        });
        if selected.is_some() {
            break;
        }
    }
    Ok(models)
}
fn allowed(models: &[ModelCapability], selection: &GenerationSelection) -> Result<()> {
    let model = models
        .iter()
        .find(|model| model.model == selection.model)
        .ok_or("model_not_available")?;
    if selection
        .effort
        .as_ref()
        .is_some_and(|effort| !model.efforts.contains(effort))
    {
        return Err("reasoning_effort_not_supported");
    }
    Ok(())
}
pub struct Authorization {
    auth: Arc<AuthManager>,
    account: String,
    session: String,
    model: String,
    epoch: u64,
    model_checked: bool,
    models: Vec<ModelCapability>,
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
            models: vec![],
        })
    }
    pub fn account(&self) -> &str {
        &self.account
    }
    pub fn session(&self) -> &str {
        &self.session
    }
    pub fn check_generation(
        &mut self,
        selection: &GenerationSelection,
        cancel: &Cancel,
    ) -> Result<()> {
        let current = self.auth.access(cancel, false)?;
        if current.account != self.account {
            return Err("auth_account_mismatch");
        }
        if current.session != self.session {
            return Err("auth_required");
        }
        let models = catalog_models(&self.auth.models(cancel)?, Some(&selection.model))?;
        allowed(&models, selection)?;
        let verified = self.auth.access(cancel, false)?;
        if verified.account != self.account {
            return Err("auth_account_mismatch");
        }
        if verified.session != current.session {
            return Err("auth_required");
        }
        self.models = models;
        Ok(())
    }
    pub fn check_request(&mut self, request: &Request, epoch: u64, cancel: &Cancel) -> Result<()> {
        if request.binding.provider != "openai-siwc" || request.binding.account != self.account {
            return Err("auth_account_mismatch");
        }
        self.check_account(epoch, cancel)?;
        if !self.model_checked
            || !self
                .models
                .iter()
                .any(|model| model.model == request.binding.requested_model)
        {
            self.models = catalog_models(
                &self.auth.models(cancel)?,
                Some(&request.binding.requested_model),
            )?;
        }
        allowed(
            &self.models,
            &GenerationSelection {
                model: request.binding.requested_model.clone(),
                effort: request.effort.clone(),
            },
        )?;
        let verified = self.auth.access(cancel, false)?;
        if verified.account != self.account {
            return Err("auth_account_mismatch");
        }
        if verified.session != self.session {
            return Err("auth_required");
        }
        self.model = request.binding.requested_model.clone();
        self.model_checked = true;
        Ok(())
    }
    /// Account/session authorization for an owned task, independent of which
    /// explicit model the next provider request selects.
    pub fn check_account(&mut self, epoch: u64, cancel: &Cancel) -> Result<()> {
        let current = self.auth.access(cancel, false)?;
        if current.account != self.account {
            return Err("auth_account_mismatch");
        }
        if epoch == self.epoch && current.session != self.session {
            return Err("auth_required");
        }
        if epoch != self.epoch {
            self.model_checked = false;
        }
        self.session = current.session;
        self.epoch = epoch;
        Ok(())
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
            let models = catalog_models(&catalog, Some(&self.model))?;
            allowed(
                &models,
                &GenerationSelection {
                    model: self.model.clone(),
                    effort: None,
                },
            )?;
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
            self.models = models;
        }
        Ok(())
    }
}
