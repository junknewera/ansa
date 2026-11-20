use super::*;

const ROLES: [&str; 3] = ["Strong", "Medium", "Weak"];

pub(super) struct RolePicker {
    models: ModelChoices,
    selections: [Option<GenerationSelection>; 3],
    role: usize,
    stage: Stage,
}

enum Stage {
    Roles,
    Models { selected: usize },
    Effort { model: usize, selected: usize },
    Saving,
}

impl RolePicker {
    pub(super) fn new(models: ModelChoices, profile: Option<RoleProfile>) -> Self {
        let selections = match profile {
            Some(profile) => [profile.strong, profile.medium, profile.weak].map(|selection| {
                models
                    .iter()
                    .any(|(model, _, efforts)| {
                        selection.model == *model
                            && selection
                                .effort
                                .as_ref()
                                .is_none_or(|value| efforts.contains(value))
                    })
                    .then_some(selection)
            }),
            None => [None, None, None],
        };
        Self {
            models,
            selections,
            role: 0,
            stage: Stage::Roles,
        }
    }

    pub(super) fn navigate(&mut self, delta: impl Fn(&mut usize, usize)) {
        match &mut self.stage {
            Stage::Roles => delta(&mut self.role, ROLES.len() + 1),
            Stage::Models { selected } => delta(selected, self.models.len()),
            Stage::Effort { model, selected } => delta(selected, self.models[*model].2.len() + 1),
            Stage::Saving => {}
        }
    }

    pub(super) fn saving(&self) -> bool {
        matches!(self.stage, Stage::Saving)
    }

    pub(super) fn back(&mut self) -> bool {
        self.stage = match self.stage {
            Stage::Effort { model, .. } => Stage::Models { selected: model },
            Stage::Models { .. } => Stage::Roles,
            Stage::Roles | Stage::Saving => return false,
        };
        true
    }

    pub(super) fn save_failed(&mut self) {
        self.stage = Stage::Roles;
    }

    fn complete(&self) -> Option<RoleProfile> {
        Some(RoleProfile {
            strong: self.selections[0].clone()?,
            medium: self.selections[1].clone()?,
            weak: self.selections[2].clone()?,
        })
    }

    pub(super) fn enter(&mut self) -> Result<Option<RoleProfile>> {
        self.stage = match self.stage {
            Stage::Roles if self.role == ROLES.len() => {
                let profile = self.complete().ok_or("orchestration_profile_incomplete")?;
                self.stage = Stage::Saving;
                return Ok(Some(profile));
            }
            Stage::Roles => Stage::Models {
                selected: self.selections[self.role]
                    .as_ref()
                    .and_then(|selection| self.models.iter().position(|m| m.0 == selection.model))
                    .unwrap_or(0),
            },
            Stage::Models { selected: model } => {
                let selected = self.selections[self.role]
                    .as_ref()
                    .filter(|choice| choice.model == self.models[model].0)
                    .and_then(|choice| choice.effort.as_ref())
                    .and_then(|choice| self.models[model].2.iter().position(|e| e == choice))
                    .map_or(0, |index| index + 1);
                Stage::Effort { model, selected }
            }
            Stage::Effort { model, selected } => {
                let (name, _, efforts) = &self.models[model];
                self.selections[self.role] = Some(GenerationSelection {
                    model: name.clone(),
                    effort: selected.checked_sub(1).map(|index| efforts[index].clone()),
                });
                self.role += 1;
                Stage::Roles
            }
            Stage::Saving => Stage::Saving,
        };
        Ok(None)
    }

    pub(super) fn popup(&self) -> Popup {
        let (subtitle, items, selected, footer) = match self.stage {
            Stage::Roles => {
                let mut items: Vec<_> = ROLES
                    .iter()
                    .zip(&self.selections)
                    .map(|(role, selection)| Choice {
                        label: (*role).into(),
                        detail: selection
                            .as_ref()
                            .map_or_else(|| "Choose a model".into(), describe),
                    })
                    .collect();
                items.push(Choice {
                    label: "Save role models".into(),
                    detail: if self.selections.iter().all(Option::is_some) {
                        String::new()
                    } else {
                        "Choose all three roles".into()
                    },
                });
                (
                    "Role models".into(),
                    items,
                    self.role,
                    "Enter select · Esc cancel",
                )
            }
            Stage::Models { selected } => (
                format!("{} · model", ROLES[self.role]),
                self.models
                    .iter()
                    .map(|(model, label, _)| Choice {
                        label: model.clone(),
                        detail: if model == label {
                            String::new()
                        } else {
                            label.clone()
                        },
                    })
                    .collect(),
                selected,
                "Enter choose effort · Esc roles",
            ),
            Stage::Effort { model, selected } => (
                format!("{} · {}", ROLES[self.role], self.models[model].0),
                std::iter::once("Default")
                    .chain(self.models[model].2.iter().map(String::as_str))
                    .map(|label| Choice {
                        label: label.into(),
                        detail: String::new(),
                    })
                    .collect(),
                selected,
                "Enter set role · Esc models",
            ),
            Stage::Saving => ("Saving role models…".into(), Vec::new(), 0, ""),
        };
        Popup {
            title: "Orchestrate+".into(),
            subtitle,
            items,
            selected,
            footer: footer.into(),
        }
    }
}

fn describe(selection: &GenerationSelection) -> String {
    let effort = selection.effort.as_deref().unwrap_or("default");
    format!("{} · {effort}", selection.model)
}

pub(super) fn profile_lines(profile: &RoleProfile) -> Vec<String> {
    ROLES
        .iter()
        .zip([&profile.strong, &profile.medium, &profile.weak])
        .map(|(role, selection)| format!("{role} · {}", describe(selection)))
        .chain(std::iter::once(String::new()))
        .collect()
}

impl Workbench {
    pub(super) fn open_role_picker(&mut self, io: &Io) -> bool {
        if !matches!(self.slots[self.current].kind, Kind::Main | Kind::Fork)
            || self.factory.orchestration_route().is_err()
        {
            self.report(
                io,
                "Orchestrate+ requires a route with verified structured outputs",
            );
            return false;
        }
        if !self.can_job(io) {
            return false;
        }
        self.cancel_modal();
        let token = self.modal_token;
        let owner = self.slots[self.current].id.clone();
        let stop = Cancel::linked(&self.stop);
        self.modal_stop = Some(stop.clone());
        let factory = self.factory.clone();
        self.menu = Some(Menu::Loading("Orchestrate+ role models".into()));
        self.jobs.spawn_blocking(move || Job::RoleModels {
            owner,
            token,
            result: (|| {
                let models = factory.plus_models(&stop)?;
                if models.is_empty() {
                    return Err("orchestration_profile_empty_catalog");
                }
                let profile = factory.role_profile()?;
                stop.check()?;
                Ok(Box::new(RolePicker::new(models, profile)))
            })(),
        });
        true
    }

    pub(super) fn enter_role_picker(&mut self, mut picker: Box<RolePicker>, io: &Io) {
        match picker.enter() {
            Ok(Some(profile)) if self.can_job(io) => {
                let owner = self.slots[self.current].id.clone();
                let token = self.modal_token;
                let factory = self.factory.clone();
                let stop = Cancel::linked(&self.stop);
                self.modal_stop = Some(stop.clone());
                self.jobs.spawn_blocking(move || Job::RoleModelsSaved {
                    owner,
                    token,
                    result: factory
                        .validate_role_profile(&profile, &stop)
                        .and_then(|()| factory.save_role_profile(&profile)),
                });
            }
            Ok(Some(_)) => picker.save_failed(),
            Err(_) => self.report(io, "Choose models and effort for all three roles"),
            Ok(None) => {}
        }
        self.menu = Some(Menu::Roles(picker));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_picker_only_offers_catalog_efforts_and_preserves_other_roles() {
        let models = vec![
            ("provider/one".into(), "One".into(), vec!["high".into()]),
            ("provider/two".into(), "Two".into(), vec!["low".into()]),
        ];
        let mut picker = RolePicker::new(models, None);
        picker.role = 3;
        assert!(picker.enter().is_err());
        picker.role = 0;
        picker.enter().unwrap();
        picker.enter().unwrap();
        picker.navigate(|selected, count| {
            assert_eq!(count, 2);
            *selected = 1;
        });
        picker.enter().unwrap();
        picker.enter().unwrap();
        picker.navigate(|selected, _| *selected = 1);
        picker.enter().unwrap();
        picker.navigate(|selected, count| {
            assert_eq!(count, 2);
            *selected = 1;
        });
        picker.enter().unwrap();
        picker.enter().unwrap();
        picker.enter().unwrap();
        picker.enter().unwrap();
        let profile = picker.enter().unwrap().unwrap();
        assert_eq!(profile.strong.effort.as_deref(), Some("high"));
        assert_eq!(profile.medium.model, "provider/two");
        assert_eq!(profile.medium.effort.as_deref(), Some("low"));
        assert_eq!(profile.weak.effort, None);
    }
}
