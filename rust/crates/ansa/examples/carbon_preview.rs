//! Reproducible visual fixture using the production renderer, without a model.
use ansa::frontend::{
    carbon::{self, Entry, Workspace},
    diff,
    theme::Theme,
};

fn main() {
    let row = |role: &str, text: &str, action: Option<&str>| Entry {
        role: role.into(),
        text: text.into(),
        action: action.map(str::to_owned),
        edit: None,
        status: text
            .find("готово")
            .map(|start| start..start + "готово".len()),
    };
    let mut history = vec![
        row("user", "Прочитай README", None),
        row(
            "tool_success",
            "> Чтение и поиск · 3 вызова · готово · README.md, docs/PROJECT_STATE.md",
            Some("group:read"),
        ),
        row(
            "tool_success",
            "> Запуск git status --short --branch · готово · 63 мс",
            Some("status"),
        ),
        row(
            "tool_success",
            "> Запуск git log -5 --oneline · готово · 50 мс",
            Some("log"),
        ),
        row(
            "tool_success",
            "v Чтение и поиск · 3 вызова · готово · README.md",
            Some("group:details"),
        ),
        row(
            "tool_success",
            "v Чтение README.md · готово · 12 мс",
            Some("read"),
        ),
        row(
            "tool_detail",
            "  Вывод:\n    # ansa\n    Локальный агент для работы с проектом.",
            None,
        ),
        row(
            "assistant",
            "Прочитал README полностью.\n\n# Как устроен проект\n- Работа с файлами, поиск и команды.\n- Сессии сохраняются между запусками.\n- Сборка и проверки закреплены через `Nix`.\n\nЗапуск: `./result/bin/ansa`.",
            None,
        ),
    ];
    let patch = diff::between(
        "# ansa\n\nЛокальный агент.\n",
        "# ansa\n\nЛокальный агент для работы с проектом.\nСессии сохраняются между запусками.\n",
    )
    .unwrap();
    history.truncate(3);
    history.push(row(
        "tool_success",
        "> Изменён README.md · (+2 -1) · готово",
        Some("diff:demo"),
    ));
    history.extend(
        patch
            .rows
            .iter()
            .map(|line| row(line.role(), &line.display(), Some("diff:demo"))),
    );
    history.push(row("assistant","README обновлён. Описание проекта и сохранения сессий теперь рядом.\n\nПроверка: `git diff --check`.",None));
    let mut frame = carbon::layout_workspace(
        &history,
        "Покажи файлы проекта".as_bytes(),
        "Покажи файлы проекта".len(),
        "basic  готов  43 с  tools 9/9  Ctrl+T",
        0,
        "~/Workspace/pet/ansa  gpt-6.1-sol",
        (104, 34),
        true,
        &Workspace {
            breadcrumb: "main / s_demo".into(),
            model: "gpt-6.1-sol".into(),
            activity: "43 с".into(),
            ..Workspace::default()
        },
    )
    .unwrap();
    frame.apply_theme(
        &Theme::builtin(&std::env::args().nth(1).unwrap_or_else(|| "carbon".into()))
            .expect("known theme"),
    );
    println!("{}", serde_json::to_string(&frame.lines).unwrap());
}
