//! Bounded user-facing status derived from the runtime snapshot.
use super::protocol_v1::{RequestId, Snapshot};
use std::time::Instant;

pub(crate) fn safe_code(code: &str) -> &str {
    if code.len() <= 64 && crate::provider::contracts::valid_id(code) {
        code
    } else {
        "runtime_failure"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    fn view() -> Snapshot {
        Snapshot {
            timeline_size: 0,
            generation: super::super::protocol_v1::GenerationSelection {
                model: "fixture-model".into(),
                effort: None,
            },
            followups: vec![],
            through_sequence: 1,
            state: "paused".into(),
            request: Some("request_1".into()),
            worker_busy: false,
            completed_tools: 0,
            text: String::new(),
            diagnostic: "http_timeout".into(),
            recovery_operation: None,
            submit: None,
            command: None,
            tools: vec![],
            history_items: 0,
            compactions: 0,
            storage_failed: false,
            durable: true,
            resume_available: true,
            mode: super::super::protocol_v1::AgentMode::Basic,
            transcript_size: 1,
            preview: None,
        }
    }
    #[test]
    fn pause_keeps_reason_and_only_offers_available_resume() {
        let mut v = view();
        let text = pause(&v);
        assert!(
            text.contains("http_timeout")
                && text.contains("таймаут")
                && text.contains("/resume - продолжить")
        );
        for code in ["http_connect", "http_transfer", "http_content_type_count"] {
            v.diagnostic = code.into();
            assert!(pause(&v).contains(code));
        }
        v.recovery_operation = Some("run:op:1".into());
        assert!(pause(&v).contains("effect_unknown"));
        assert!(!pause(&v).contains("/resume - продолжить"));
        v.state = "cancelled".into();
        assert!(needs_attention(&v));
        assert!(pause(&v).contains("effect_unknown"));
        v.storage_failed = true;
        v.diagnostic = "journal_checksum".into();
        assert!(pause(&v).contains("Ошибка хранения"));
        assert!(pause(&v).contains("journal_checksum"));
        for code in ["secret\n\u{1b}[31m", &"x".repeat(10000)] {
            v.diagnostic = code.into();
            let text = pause(&v);
            assert!(!text.contains(code));
            assert!(!text.contains('\u{1b}'));
            assert!(text.len() < 2048);
        }
        for (storage, diagnostic, recovery, expected) in [
            (false, "http_timeout", None, "http_timeout"),
            (false, "http_timeout", Some("run:op:1"), "effect_unknown"),
            (
                true,
                "journal_checksum",
                Some("run:op:1"),
                "journal_checksum",
            ),
        ] {
            v.storage_failed = storage;
            v.diagnostic = diagnostic.into();
            v.recovery_operation = recovery.map(Into::into);
            let status = format!(
                "basic\n{}\n{}",
                pause_notice(&v),
                status_notice(&v, "/resume недоступен")
            );
            for cols in [20, 80] {
                let frame = super::super::carbon::layout_full(
                    &[],
                    b"draft",
                    2,
                    &status,
                    0,
                    "",
                    (cols, 10),
                    false,
                )
                .unwrap();
                let screen = frame.lines.join("\n");
                assert_eq!(screen.matches(expected).count(), 1, "{screen}");
                if recovery.is_some() {
                    assert_eq!(screen.matches("effect_unknown").count(), 1, "{screen}");
                }
            }
        }
    }
    #[test]
    fn refresh_does_not_restart_stage_elapsed_or_redraw_subseconds() {
        let start = Instant::now();
        let mut v = view();
        v.state = "generating".into();
        v.worker_busy = true;
        let mut clock = WaitClock::default();
        clock.observe(&v, start);
        clock.observe(&v, start + Duration::from_secs(1));
        assert!(clock.tick(start + Duration::from_secs(3)));
        assert!(clock.text().unwrap().contains("3 с"));
        assert!(!clock.tick(start + Duration::from_millis(3100)));
        v.request = Some("request_2".into());
        clock.observe(&v, start + Duration::from_secs(4));
        assert!(clock.text().unwrap().contains("0 с"));
        v.state = "executing".into();
        clock.observe(&v, start + Duration::from_secs(5));
        assert!(clock.text().unwrap().contains("Выполнение инструмента"));
        v.worker_busy = false;
        clock.observe(&v, start + Duration::from_secs(6));
        assert!(clock.text().is_none());
    }
}
pub(crate) fn error(code: &str) -> String {
    let code = safe_code(code);
    format!("{} ({code})", explanation(code))
}
fn explanation(code: &str) -> &'static str {
    match code {
        "http_timeout" => "Истёк таймаут HTTP-запроса",
        "http_connect" | "http_handshake" => "Не удалось установить соединение с сервером",
        "http_transfer" => "Соединение оборвалось или ответ получен не полностью",
        "http_content_type_count"
        | "http_content_type_missing"
        | "http_mime"
        | "http_mime_parameter" => "Сервер вернул недопустимый Content-Type для Responses",
        "auth_required" => "Требуется восстановить вход в аккаунт",
        "provider_policy_denied" => "Провайдер запретил запрос",
        "provider_usage_limit" => "Достигнут лимит использования провайдера",
        "session_interrupted" => "Незавершённый запрос восстановлен после перезапуска",
        "opaque_conflict" => "Зашифрованный блок ответа изменился во время получения",
        "cli_output_stalled" => "Приём вывода остановился; сообщение передано не полностью",
        "cli_output_interrupted" => "Передача вывода прервана пользователем",
        "cli_output" => "Не удалось завершить передачу вывода",
        _ => "Ошибка выполнения",
    }
}
pub(crate) fn pause_notice(view: &Snapshot) -> String {
    if view.storage_failed {
        return format!("{}: Ошибка хранения сессии", safe_code(&view.diagnostic));
    }
    if view.recovery_operation.is_some() {
        return "effect_unknown: Результат инструмента неизвестен".into();
    }
    if view.diagnostic.is_empty() {
        return "runtime_failure: Причина паузы не сообщена".into();
    }
    let code = safe_code(&view.diagnostic);
    format!("{code}: {}", explanation(code))
}
pub(crate) fn needs_attention(view: &Snapshot) -> bool {
    view.state == "paused" || view.storage_failed || view.recovery_operation.is_some()
}
pub(crate) fn status_notice(view: &Snapshot, detail: &str) -> String {
    if view.storage_failed && view.recovery_operation.is_some() {
        format!("effect_unknown: {detail}")
    } else {
        detail.to_owned()
    }
}
pub(crate) fn pause(view: &Snapshot) -> String {
    let mut text = if view.storage_failed {
        "Ошибка хранения сессии".to_owned()
    } else if view.state == "paused" {
        "Сессия приостановлена".to_owned()
    } else {
        "Требуется проверка результата операции".to_owned()
    };
    if !view.diagnostic.is_empty() {
        text.push_str(": ");
        text.push_str(&error(&view.diagnostic));
    } else if view.state == "paused" {
        text.push_str(": причина не сообщена runtime");
    }
    text.push('.');
    if view.recovery_operation.is_some() {
        text.push_str(" Результат инструмента неизвестен (effect_unknown). Сначала проверьте внешний эффект; повтор операции запрещён.");
    }
    if view.resume_available && !view.storage_failed && view.recovery_operation.is_none() {
        text.push_str(" /resume - продолжить; /cancel - отменить запрос.");
    } else {
        text.push_str(" /resume пока недоступен.");
        if view.state == "paused" && view.request.is_some() {
            text.push_str(" /cancel - отменить запрос.");
        }
    }
    text
}

#[derive(Default)]
pub(crate) struct WaitClock {
    stage: String,
    request: Option<RequestId>,
    started: Option<Instant>,
    seconds: u64,
}
impl WaitClock {
    pub fn observe(&mut self, view: &Snapshot, now: Instant) {
        if !view.worker_busy {
            self.started = None;
            return;
        }
        if self.started.is_none() || self.stage != view.state || self.request != view.request {
            self.stage = view.state.clone();
            self.request = view.request.clone();
            self.started = Some(now);
            self.seconds = 0;
        }
    }
    pub fn tick(&mut self, now: Instant) -> bool {
        let Some(started) = self.started else {
            return false;
        };
        let seconds = now.saturating_duration_since(started).as_secs();
        let changed = seconds != self.seconds;
        self.seconds = seconds;
        changed
    }
    pub fn text(&self) -> Option<String> {
        self.started?;
        let stage = match self.stage.as_str() {
            "loading" => "Открытие сессии",
            "generating" => "Ожидание ответа",
            "compacting" => "Сжатие контекста",
            "executing" => "Выполнение инструмента",
            "persisting" | "finishing" => "Сохранение результата",
            _ => "Ожидание завершения операции",
        };
        Some(format!("{stage}: {} с", self.seconds))
    }
}
