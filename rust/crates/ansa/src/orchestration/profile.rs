use crate::{
    protocol::json::Json,
    provider::contracts::{GenerationSelection, valid_id},
};

/// The role selections captured for an Orchestrate+ turn or proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleProfile {
    pub strong: GenerationSelection,
    pub medium: GenerationSelection,
    pub weak: GenerationSelection,
}

impl RoleProfile {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        for selection in [&self.strong, &self.medium, &self.weak] {
            if selection.model.is_empty()
                || selection.model.len() > 256
                || selection.model.chars().any(char::is_control)
            {
                return Err("orchestration_profile_model");
            }
            if selection
                .effort
                .as_ref()
                .is_some_and(|effort| !valid_id(effort))
            {
                return Err("orchestration_profile_effort");
            }
        }
        Ok(())
    }

    pub(crate) fn encode(&self) -> Result<Json, &'static str> {
        fn selection(value: &GenerationSelection) -> Result<Json, &'static str> {
            Json::object(vec![
                ("model", Json::string(&value.model)?),
                (
                    "effort",
                    value
                        .effort
                        .as_deref()
                        .map_or_else(|| Json::parse("null"), Json::string)?,
                ),
            ])
        }
        Json::object(vec![
            ("version", Json::parse("1")?),
            ("strong", selection(&self.strong)?),
            ("medium", selection(&self.medium)?),
            ("weak", selection(&self.weak)?),
        ])
    }

    pub(crate) fn decode(value: &Json) -> Result<Self, &'static str> {
        fn selection(value: &Json) -> Result<GenerationSelection, &'static str> {
            let model = value.text_field("model")?;
            let effort = value.field("effort")?;
            let effort = if effort.is_null() {
                None
            } else {
                let effort = effort.text()?;
                Some(effort.to_owned())
            };
            Ok(GenerationSelection {
                model: model.to_owned(),
                effort,
            })
        }
        if value.usize_field("version")? != 1 {
            return Err("orchestration_profile_version");
        }
        let profile = Self {
            strong: selection(value.field("strong")?)?,
            medium: selection(value.field("medium")?)?,
            weak: selection(value.field("weak")?)?,
        };
        profile.validate()?;
        Ok(profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_round_trips_role_models_and_efforts() {
        let selection = |model: &str, effort: Option<&str>| GenerationSelection {
            model: model.to_owned(),
            effort: effort.map(str::to_owned),
        };
        let profile = RoleProfile {
            strong: selection("provider/strong", Some("high")),
            medium: selection("provider/medium", Some("medium")),
            weak: selection("provider/weak", None),
        };
        let encoded = profile.encode().unwrap();
        assert_eq!(RoleProfile::decode(&encoded).unwrap(), profile);
        assert!(RoleProfile::decode(&encoded.without("weak").unwrap()).is_err());
        assert!(
            RoleProfile::decode(&encoded.with("version", Json::parse("2").unwrap()).unwrap())
                .is_err()
        );
    }
}
