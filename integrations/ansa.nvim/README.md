# ansa.nvim

Локальный viewer разговора Carbon. Текущий implementation WIP относится к
[PR #26](https://github.com/junknewera/ansa/pull/26).

В основном терминале:

```sh
ansa --workspace-dir /run/user/$(id -u)/ansa-workspace
```

В соседнем Neovim добавьте эту папку в runtimepath и включите команды:

```lua
vim.opt.runtimepath:prepend('/path/to/ansa/integrations/ansa.nvim')
require('ansa').setup()
```

```vim
:AnsaAttach /run/user/1000/ansa-workspace/workspace.sock
:AnsaHistory
```

Используйте свой UID и путь. Каталог endpoint должен принадлежать пользователю
и иметь права `0700`; socket создаётся с `0600`. Занятый socket не удаляется
автоматически. Endpoint привязан к исходному разговору Carbon.

`:AnsaRefresh` явно обновляет историю, `:AnsaDetach` отключает viewer.
Отключение сохраняет открытый документ и не отменяет работу агента. Обновления
сохраняют положение чтения; если буфер изменён, новая версия доступна отдельно.
Plugin не записывает файл и не повторяет задачи при reconnect.

Клиент требует Neovim с `vim.uv`, `vim.json` и `nvim_exec2` (0.10+).
Адресные проверки выполнены на настоящем Neovim с чистой конфигурацией.
Markdown renderer, изображения и editor context добавляются следующим slice.
