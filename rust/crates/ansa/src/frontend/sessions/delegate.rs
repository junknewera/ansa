use super::*;
use crate::{
    orchestration::plan::{Access, Step},
    runtime::{ChildAttemptState, CommandPayload, PostStatus, RuntimeCommand},
    tools::registry::Outcome,
};

impl Factory {
    pub(in crate::frontend) fn enable_delegation(&self, options: &mut Options, parent: &str) {
        if self.orchestration_route().is_ok() {
            let factory = self.clone();
            let parent = parent.to_owned();
            options.delegate = Some(Arc::new(move |source, roles, args, stop| {
                factory.delegate(&parent, source, roles, args, stop)
            }));
        }
    }
    fn delegate(
        &self,
        parent: &str,
        source: &Request,
        roles: Option<&crate::orchestration::profile::RoleProfile>,
        args: &Json,
        stop: &Cancel,
    ) -> Outcome {
        let start = || -> Result<_> {
            stop.check()?;
            let (task, paths) = crate::tools::delegation::assignment(args, &source.tools)?;
            let read = paths.is_empty();
            let step = Step {
                id: "delegate".into(),
                task: task.into(),
                access: if read { Access::Read } else { Access::Write },
                paths,
                depends_on: vec![],
                acceptance: String::new(),
            };
            let selected = roles
                .map(|roles| if read { &roles.weak } else { &roles.medium })
                .map(|generation| self.role_request(source, generation))
                .transpose()?;
            let source = selected.as_ref().unwrap_or(source);
            self.require_orchestration_route(source, stop)?;
            let reactor = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .map_err(|_| "delegation_runtime")?;
            let child = self.create_orchestration_child(
                parent,
                &self.settings.cwd,
                &step,
                source,
                None,
                &[],
                ChildKind::Direct,
                stop,
            )?;
            let line = super::super::carbon::AgentLine {
                id: child.selected.id.clone(),
                title: step
                    .task
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .chars()
                    .take(96)
                    .collect(),
                state: "Running".into(),
                role: super::super::carbon::AgentRole::worker(step.access),
                model: source.binding.requested_model.clone(),
                activity: String::new(),
                step: None,
                tool_calls: None,
            };
            Ok((reactor, child, step, line))
        };
        let (reactor, child, step, line) = match start() {
            Ok(value) => value,
            Err(code) => return Outcome::failed_known(code.to_owned()),
        };
        let id = child.selected.id.clone();
        self.live_delegates.send_modify(|map| {
            map.insert(id.clone(), (parent.to_owned(), line));
        });
        let result = reactor.block_on(run_child(child, step, stop));
        self.live_delegates.send_modify(|map| {
            map.remove(&id);
        });
        result
    }
}

async fn run_child(mut child: Created, step: Step, stop: &Cancel) -> Outcome {
    let handle = child.runtime.handle();
    let submit = crate::runtime::Submit {
        text: step.task,
        mode: if step.access == Access::Read {
            crate::runtime::AgentMode::Plan
        } else {
            crate::runtime::AgentMode::Basic
        },
    };
    let command = RuntimeCommand {
        schema_version: 1,
        session: handle.session().clone(),
        incarnation: handle.incarnation().clone(),
        command: "assignment".into(),
        turn: "task".into(),
        payload: CommandPayload::Submit(submit.clone()),
    };
    let ready = child.runtime.ready().await.and_then(|()| stop.check());
    if let Err(code) = ready {
        return Outcome::failed_known(code.to_owned());
    }
    let mut cancel_sent = false;
    let result = match child.runtime.post(command.clone()).await {
        Ok(PostStatus::Accepted) => loop {
            let receipt = match handle
                .child_attempt(
                    command.command.clone(),
                    command.turn.clone(),
                    submit.clone(),
                )
                .await
            {
                Ok(receipt) => receipt,
                Err(code) => break Err(code),
            };
            if matches!(receipt.state(), ChildAttemptState::Missing) {
                return Outcome::failed_known("delegate_submit_rejected".into());
            }
            if stop.cancelled()
                && !cancel_sent
                && let ChildAttemptState::Durable { request, state, .. } = receipt.state()
                && !matches!(state.as_str(), "completed" | "paused" | "cancelled")
            {
                let cancel = RuntimeCommand {
                    command: "stop-assignment".into(),
                    payload: CommandPayload::Cancel(request.clone()),
                    ..command.clone()
                };
                match handle.post(cancel).await {
                    Ok(PostStatus::Accepted | PostStatus::Duplicate) => cancel_sent = true,
                    Ok(PostStatus::StaleTarget | PostStatus::QueueFull) => {}
                    _ => break Err("delegate_cancel_failed"),
                }
            }
            if let ChildAttemptState::Durable {
                state,
                quiescent: true,
                recovery_required,
                resume_required,
                final_response,
                ..
            } = receipt.state()
            {
                if *recovery_required {
                    break Err("delegate_effect_unknown");
                }
                if state == "completed" {
                    break final_response
                        .as_ref()
                        .ok_or("delegate_result_missing")
                        .and_then(|response| response.child_result())
                        .and_then(|report| {
                            serde_json::to_string(&report).map_err(|_| "delegate_result")
                        });
                }
                if *resume_required || state == "paused" || state == "cancelled" {
                    let diagnostic = receipt.pause_diagnostic().unwrap_or(state);
                    child.runtime.close_input();
                    if child.runtime.join().await.is_err() {
                        break Err("delegate_join");
                    }
                    return Outcome::failed_known(format!(
                        "{diagnostic} (session {})",
                        child.selected.id
                    ));
                }
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        },
        Ok(_) => return Outcome::failed_known("delegate_submit_rejected".into()),
        Err(code) => Err(code),
    };
    let joined = child.runtime.shutdown().await;
    match result.and_then(|report| joined.map(|()| report)) {
        Ok(report) => Outcome::succeeded(report),
        Err(code) if step.access == Access::Read => Outcome::failed_known(code.into()),
        Err(code) => Outcome::from_bytes(
            crate::provider::replay::Execution::Failed,
            crate::provider::replay::Effects::Unknown,
            format!("{code} (session {})", child.selected.id).into_bytes(),
        ),
    }
}
