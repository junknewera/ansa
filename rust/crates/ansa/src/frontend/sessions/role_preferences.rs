use super::*;

impl Factory {
    fn role_preferences_directory(&self) -> PathBuf {
        self.generation_directory().join("orchestrate-plus")
    }

    pub(in crate::frontend) fn role_profile(
        &self,
    ) -> Result<Option<crate::orchestration::profile::RoleProfile>> {
        let directory = match ansa_linux::fs::private_directory(&self.role_preferences_directory())
        {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("orchestration_profile_directory"),
        };
        let bytes = ansa_linux::local::read(&directory, "generation.json", 4096)
            .map_err(|_| "orchestration_profile_read")?;
        bytes
            .map(|bytes| {
                let text = std::str::from_utf8(&bytes).map_err(|_| "orchestration_profile_read")?;
                let value = Json::parse_with_limits(text, 4096, 8, 64)?;
                crate::orchestration::profile::RoleProfile::decode(&value)
            })
            .transpose()
    }

    pub(in crate::frontend) fn validate_role_profile(
        &self,
        profile: &crate::orchestration::profile::RoleProfile,
        stop: &Cancel,
    ) -> Result<()> {
        if let Some(route) = &self.openrouter {
            for selection in [&profile.strong, &profile.medium, &profile.weak] {
                route.validate_role(selection, stop)?;
            }
        }
        stop.check()
    }
    pub(in crate::frontend) fn save_role_profile(
        &self,
        profile: &crate::orchestration::profile::RoleProfile,
    ) -> Result<()> {
        let path = self.role_preferences_directory();
        ansa_linux::fs::create_private(&path).map_err(|_| "orchestration_profile_directory")?;
        let directory = ansa_linux::fs::private_directory(&path)
            .map_err(|_| "orchestration_profile_directory")?;
        ansa_linux::local::replace_generation(
            &directory,
            profile.encode()?.dump().as_bytes(),
            &crate::auth::pkce::random_token(16)?.replace('_', "-"),
        )
        .map_err(|_| "orchestration_profile_write")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{auth::Secret, orchestration::profile::RoleProfile};

    #[test]
    fn saved_roles_are_separate_for_siwc_and_openrouter() {
        struct TestDirectory(PathBuf);
        impl Drop for TestDirectory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = TestDirectory(std::env::temp_dir().join(format!(
            "ansa-role-profile-{}",
            crate::auth::pkce::random_token(16).unwrap()
        )));
        ansa_linux::fs::create_private(&root.0).unwrap();
        let settings = crate::config::load_agent_settings(&root.0, &root.0).unwrap();
        let route = Arc::new(
            OpenRouter::new(
                Secret::new("synthetic-key-for-persistence".into()),
                Arc::new(|_, _| unreachable!("preferences do not use the network")),
            )
            .unwrap(),
        );
        let account = route.binding().account.clone();
        let factory = || {
            Factory::new(
                settings.clone(),
                root.0.join("auth"),
                false,
                account.clone(),
                None,
            )
        };
        let siwc = factory();
        let router = factory().with_openrouter(route.clone());
        let profile = |model: &str| {
            let selection = GenerationSelection {
                model: model.into(),
                effort: None,
            };
            RoleProfile {
                strong: selection.clone(),
                medium: selection.clone(),
                weak: selection,
            }
        };
        assert_eq!(siwc.role_profile().unwrap(), None);
        assert_eq!(router.role_profile().unwrap(), None);
        siwc.save_role_profile(&profile("chat-model")).unwrap();
        assert_eq!(router.role_profile().unwrap(), None);
        router
            .save_role_profile(&profile("provider/router"))
            .unwrap();
        assert_eq!(
            factory().role_profile().unwrap(),
            Some(profile("chat-model"))
        );
        assert_eq!(
            factory().with_openrouter(route).role_profile().unwrap(),
            Some(profile("provider/router"))
        );
    }
}
