use crate::{
    protocol::json::Json,
    provider::contracts::{GenerationSelection, valid_id},
};

/// The role selections captured when an Orchestrate+ proposal is prepared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleProfile {
    pub strong: GenerationSelection,
    pub medium: GenerationSelection,
    pub weak: GenerationSelection,
}

impl RoleProfile {
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
            if model.is_empty() || model.len() > 256 || model.chars().any(char::is_control) {
                return Err("orchestration_profile_model");
            }
            let effort = value.field("effort")?;
            let effort = if effort.is_null() {
                None
            } else {
                let effort = effort.text()?;
                if !valid_id(effort) {
                    return Err("orchestration_profile_effort");
                }
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
        Ok(Self {
            strong: selection(value.field("strong")?)?,
            medium: selection(value.field("medium")?)?,
            weak: selection(value.field("weak")?)?,
        })
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
