# TODO: blackbox behavior logger

## Цель

Добавить в `mhd` минимальный модуль самоконтроля `blackbox`, который пишет
дневной append-only лог активности за компьютером.

Модуль не анализирует продуктивность, не делает summary, не классифицирует окна
и не пишет содержимое пользовательского ввода. Его задача - сухо фиксировать
изменение контекста и границы рабочих сессий.

## Имя и расположение логов

- Имя модуля: `blackbox`.
- Директория логов:

```text
%USERPROFILE%\.config\mhd\blackbox
```

- Файл на каждый день:

```text
%USERPROFILE%\.config\mhd\blackbox\YYYY-MM-DD.log
```

- При смене даты новый event пишется уже в файл новой даты.
- Старый файл не закрывается отдельным событием из-за смены даты.
- Логи только дописываются.
- Не нужен lock-файл.
- Не нужен `.current`.
- Не нужен анализ предыдущих логов при старте.

## Формат строк

Одна строка = одно событие.

Формат:

```text
YYYY-MM-DD HH:MM:SS event=<event_name> key=value key="escaped value"
```

Примеры:

```text
2026-05-22 09:12:03 event=monitoring_started
2026-05-22 09:12:08 event=virtual_desktop_changed name="Work"
2026-05-22 09:12:10 event=window_changed title="Visual Studio Code - mhd"
2026-05-22 09:12:11 event=session_started
2026-05-22 09:36:26 event=session_ended duration_sec=1455 actions=1847 keyboard=1320 mouse=527 idle_sec=300
2026-05-22 10:15:00 event=monitoring_stopped reason="quit"
```

Правила форматирования:

- Timestamp локальный, точность до секунд.
- Timestamp без timezone.
- Значения с пробелами писать в кавычках.
- В строковых значениях экранировать:
  - `\` как `\\`
  - `"` как `\"`
  - newline/carriage return заменять на пробел или `\n`/`\r`; выбрать один вариант при реализации и покрыть тестом.
- Числа писать без кавычек.
- Event names в `snake_case`.

## События

### `monitoring_started`

Пишется один раз при старте мониторинга `blackbox`.

```text
2026-05-22 09:12:03 event=monitoring_started
```

Открытые вопросы реализации:

- Если `blackbox` включается конфигом, писать событие после успешной инициализации директории логов.
- Если лог-файл создать нельзя, модуль должен отключиться с ошибкой в stderr/tray diagnostics, но не валить весь `mhd`.

### `monitoring_stopped`

Пишется при штатной остановке мониторинга.

```text
2026-05-22 10:15:00 event=monitoring_stopped reason="quit"
```

Минимальные причины:

- `quit` - пользователь/действие `quit` завершило `mhd`.
- `shutdown` - штатное завершение процесса/daemon lifecycle, если можно отличить от quit.
- `disabled` - мониторинг выключен конфигом при reload, если reload будет поддержан.

Не делать:

- Не анализировать предыдущий лог.
- Не дописывать `previous monitoring ended unexpectedly`.
- Не пытаться восстанавливать состояние после crash.

### `window_changed`

Пишется при изменении активного foreground window title.

```text
2026-05-22 09:12:10 event=window_changed title="Visual Studio Code - mhd"
```

Требования:

- Не стартует рабочую сессию.
- Не увеличивает счетчик действий.
- Не писать повторно, если title не изменился.
- Если title пустой, допустимо писать `title=""` или пропустить событие; выбрать один вариант и зафиксировать тестом.
- Не логировать process name на первом этапе.
- Не логировать executable path.
- Не логировать URL браузера.

Практическая реализация:

- Использовать Win32 foreground window tracking.
- Предпочтительно event-driven подход через WinEvent hook:
  - `SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, ...)`
  - out-of-context callback.
- После foreground event получить title через `GetWindowTextW`.
- Если event-driven подход окажется нестабильным, допустим аккуратный polling с большим интервалом, но это хуже для идеи `dry and fit`.

### `virtual_desktop_changed`

Пишется при смене virtual desktop.

```text
2026-05-22 09:12:08 event=virtual_desktop_changed name="Work"
```

Требования:

- Не стартует рабочую сессию.
- Не увеличивает счетчик действий.
- Не писать повторно, если desktop не изменился.
- Если имя desktop получить нельзя, писать стабильный идентификатор:

```text
2026-05-22 09:12:08 event=virtual_desktop_changed id="{guid-or-index}"
```

Открытые вопросы реализации:

- На Windows virtual desktop API нестабилен и частично COM/private.
- Нужен отдельный spike перед основной реализацией:
  - проверить доступный crate/API;
  - понять, можно ли получить имя desktop;
  - понять, можно ли получать события без polling.

Приоритет:

- Если получение virtual desktop сложно, сначала реализовать `window_changed` и sessions.
- `virtual_desktop_changed` добавить вторым этапом.

### `session_started`

Пишется при первом засчитанном действии после idle/старта мониторинга.

```text
2026-05-22 09:12:11 event=session_started
```

Засчитанные действия:

- keyboard button press;
- mouse button press;
- mouse wheel / scroll.

Не засчитывать:

- mouse move;
- window change;
- virtual desktop change;
- key up / mouse button up, если hook получает release events.

Требования:

- `session_started` timestamp = время первого засчитанного действия.
- Повторные действия внутри активной сессии не пишут новые строки.
- Счетчики текущей сессии инициализируются:
  - `keyboard=0`
  - `mouse=0`
  - `actions=0`
- Первое действие входит в счетчик.

### `session_ended`

Пишется после того, как 5 минут не было засчитанных действий.

```text
2026-05-22 09:36:26 event=session_ended duration_sec=1455 actions=1847 keyboard=1320 mouse=527 idle_sec=300
```

Важное правило timestamp:

- Физически строка дописывается после 5 минут idle.
- Timestamp в строке ставится на момент начала бездействия.
- То есть timestamp = время последнего засчитанного действия, а не время записи строки.

Требования:

- `duration_sec` = `last_action_at - session_started_at`.
- `duration_sec` не включает idle.
- `idle_sec=300`.
- `actions = keyboard + mouse`.
- Если сессия состоит из одного действия, `duration_sec=0`.
- Если мониторинг останавливается во время активной сессии, перед `monitoring_stopped` надо записать `session_ended` с timestamp последнего действия и текущими счетчиками.

Открытый нюанс:

- При остановке во время активной сессии `idle_sec` уже не равен 300.
- Варианты:
  - писать `idle_sec=0 reason="stop"`;
  - писать `reason="stop"` без `idle_sec`;
  - не закрывать сессию при остановке.
- Рекомендация: закрывать сессию так:

```text
2026-05-22 10:14:58 event=session_ended duration_sec=372 actions=410 keyboard=390 mouse=20 reason="stop"
2026-05-22 10:15:00 event=monitoring_stopped reason="quit"
```

## Session state machine

Состояния:

- `Idle`
- `ActiveSession`

### `Idle`

Храним:

- `last_window_title`
- `last_virtual_desktop`
- no active counters

Переходы:

- counted keyboard/mouse action -> `ActiveSession`
  - write `session_started`
  - set `session_started_at = action_time`
  - set `last_action_at = action_time`
  - increment keyboard/mouse counter
- window/desktop change -> stay `Idle`
  - write corresponding context event if value changed
- stop monitoring -> write `monitoring_stopped`

### `ActiveSession`

Храним:

- `session_started_at`
- `last_action_at`
- `keyboard_count`
- `mouse_count`
- `idle_deadline = last_action_at + 300 sec`

Переходы:

- counted keyboard/mouse action before idle deadline -> stay `ActiveSession`
  - update `last_action_at`
  - increment counter
  - refresh idle deadline
- idle deadline reached -> `Idle`
  - write `session_ended` using timestamp `last_action_at`
- window/desktop change -> stay `ActiveSession`
  - write corresponding context event if value changed
  - do not change `last_action_at`
  - do not increment counters
- stop monitoring -> `Idle`
  - write `session_ended` with `reason="stop"` if active
  - write `monitoring_stopped`

## Интеграция с существующим `mhd`

Предполагаемая структура:

```text
mhd-daemon/src/
├── blackbox.rs
└── ...
```

Если модуль разрастется:

```text
mhd-daemon/src/blackbox/
├── mod.rs
├── logger.rs
├── session.rs
├── window.rs
└── virtual_desktop.rs
```

Первый этап лучше держать в одном `blackbox.rs`, пока не появится реальная
сложность.

Интеграционные точки:

- `main.rs` / `app.rs`:
  - стартовать `blackbox` вместе с daemon lifecycle;
  - останавливать при quit/shutdown.
- `hook.rs`:
  - передавать в `blackbox` только counted actions:
    - keyboard press;
    - mouse button press;
    - mouse wheel.
  - не передавать mouse move.
- foreground window tracking:
  - отдельный lightweight thread or WinEvent callback;
  - сообщения уходят в `blackbox` через channel.
- virtual desktop tracking:
  - отдельный компонент после spike.

Важно:

- Не добавлять тяжелые framework dependencies.
- Не блокировать low-level hook callback на file I/O.
- Не писать лог напрямую из hook hot path.
- Hook hot path должен только отправить компактное событие в channel или обновить lock-free счетчик, если так уже устроена архитектура.

## Потоки и производительность

Цель: сохранить принцип `mhd` - 0% CPU at idle.

Рекомендуемая модель:

- `blackbox` worker thread:
  - принимает события через channel;
  - владеет session state;
  - владеет file writer;
  - отвечает за idle timer.
- hook thread:
  - отправляет `InputAction { kind, timestamp }`;
  - не делает форматирование строк;
  - не делает file I/O.
- window event callback/thread:
  - отправляет `WindowChanged { title, timestamp }`.

Idle detection:

- Не polling каждую секунду.
- При каждом counted action обновлять deadline.
- Worker ожидает:
  - новое событие из channel;
  - timeout до текущего idle deadline.
- Если timeout сработал и сессия еще активна, пишет `session_ended`.

## File writer

Требования:

- Создать директорию `~\.config\mhd\blackbox`, если ее нет.
- Открывать файл текущей даты в append mode.
- Перед каждой записью проверять дату timestamp события.
- Если дата события отличается от текущего открытого файла:
  - flush текущий writer;
  - открыть файл новой даты;
  - продолжить запись.
- Flush:
  - после `monitoring_started`;
  - после `monitoring_stopped`;
  - после `session_ended`;
  - для остальных событий можно полагаться на buffering или flush по строке; выбрать после оценки простоты.

Ошибка записи:

- Не паниковать.
- Зафиксировать ошибку в diagnostics/stderr, если такой механизм уже есть.
- Можно отключить `blackbox` до следующего старта, чтобы не спамить ошибками.

## Config

Минимальный конфиг:

```toml
[blackbox]
enabled = false
idle_seconds = 300
```

Рекомендация:

- По умолчанию `enabled = false`, потому что это поведенческий лог.
- `idle_seconds` оставить настраиваемым для тестов и будущего пользователя.
- В runtime логике считать 300 дефолтом.

Опционально позже:

```toml
[blackbox]
enabled = true
idle_seconds = 300
log_dir = "C:\\Users\\name\\.config\\mhd\\blackbox"
```

Но на первом этапе лучше не добавлять `log_dir`, чтобы не расширять поверхность.

## Privacy boundary

Модуль должен логировать:

- event names;
- timestamps;
- active window title;
- virtual desktop name/id;
- aggregate keyboard count;
- aggregate mouse count.

Модуль не должен логировать:

- конкретные клавиши;
- введенный текст;
- mouse coordinates;
- mouse movement;
- clipboard;
- URL;
- process executable path;
- screenshots;
- application content.

## Implementation plan

### Phase 1 - core logger and sessions

- [ ] Добавить config section `[blackbox]`.
- [ ] Добавить модуль `blackbox`.
- [ ] Реализовать path builder:
  - `%USERPROFILE%\.config\mhd\blackbox`
  - daily `YYYY-MM-DD.log`.
- [ ] Реализовать line formatter:
  - timestamp;
  - event name;
  - key/value pairs;
  - escaping quoted strings.
- [ ] Реализовать append writer with daily rollover.
- [ ] Реализовать session state machine:
  - idle -> active;
  - active -> idle by timeout;
  - active -> stop flush.
- [ ] Подключить keyboard press accounting из hook.
- [ ] Подключить mouse button press accounting из hook.
- [ ] Подключить mouse wheel accounting из hook.
- [ ] Убедиться, что mouse move игнорируется.
- [ ] На startup писать `monitoring_started`.
- [ ] На shutdown/quit писать `monitoring_stopped`.

### Phase 2 - active window events

- [ ] Добавить foreground window watcher.
- [ ] Получать active window title через Win32.
- [ ] Дедуплицировать одинаковые title.
- [ ] Писать `window_changed`.
- [ ] Проверить, что window changes не стартуют session.
- [ ] Проверить, что window changes не влияют на idle timer.

### Phase 3 - virtual desktop events

- [ ] Провести spike по Windows virtual desktop API.
- [ ] Выбрать способ получить name/id.
- [ ] Выбрать способ отслеживать изменения без CPU-heavy polling.
- [ ] Добавить `virtual_desktop_changed`.
- [ ] Дедуплицировать одинаковые desktop values.
- [ ] Если имя недоступно, писать `id=...`.

### Phase 4 - tests and verification

- [ ] Unit tests for line escaping.
- [ ] Unit tests for session duration calculation.
- [ ] Unit tests for idle timestamp rule:
  - line is written after timeout;
  - timestamp equals `last_action_at`.
- [ ] Unit tests for daily rollover.
- [ ] Unit tests for stop during active session.
- [ ] Manual run:
  - start `mhd`;
  - press keyboard/mouse;
  - wait idle timeout;
  - verify `session_ended`.
- [ ] Manual run:
  - switch windows;
  - verify deduplicated `window_changed`.
- [ ] Manual run:
  - quit `mhd`;
  - verify `monitoring_stopped`.

## Acceptance criteria

- `blackbox` can be enabled by config.
- On start, log file for current date is created if missing.
- `monitoring_started` is appended.
- First keyboard/mouse button/wheel action appends `session_started`.
- Keyboard/mouse actions update counters without writing per-action lines.
- Mouse movement does nothing.
- After 300 seconds without counted actions, `session_ended` is appended.
- `session_ended` timestamp equals the last counted action timestamp.
- `duration_sec` excludes idle time.
- `actions = keyboard + mouse`.
- Window changes append deduplicated `window_changed`.
- Virtual desktop changes append deduplicated `virtual_desktop_changed`, if implemented in that phase.
- Quit/shutdown appends `monitoring_stopped`.
- Active session is closed before `monitoring_stopped`.
- Date rollover writes subsequent events to the new `YYYY-MM-DD.log`.
- Hook hot path does not perform file I/O.
- `mhd` remains lightweight at idle.

## Example full day fragment

```text
2026-05-22 09:12:03 event=monitoring_started
2026-05-22 09:12:08 event=virtual_desktop_changed name="Work"
2026-05-22 09:12:10 event=window_changed title="Visual Studio Code - mhd"
2026-05-22 09:12:11 event=session_started
2026-05-22 09:24:31 event=window_changed title="PowerShell"
2026-05-22 09:28:44 event=window_changed title="Mozilla Firefox - GitHub"
2026-05-22 09:35:12 event=window_changed title="Visual Studio Code - mhd"
2026-05-22 09:36:26 event=session_ended duration_sec=1455 actions=1847 keyboard=1320 mouse=527 idle_sec=300
2026-05-22 09:49:03 event=session_started
2026-05-22 10:03:19 event=virtual_desktop_changed name="Docs"
2026-05-22 10:03:21 event=window_changed title="Obsidian - Notes"
2026-05-22 10:11:42 event=session_ended duration_sec=1359 actions=923 keyboard=811 mouse=112 idle_sec=300
2026-05-22 10:15:00 event=monitoring_stopped reason="quit"
```

## Не делать в этом модуле

- Не делать day summary.
- Не делать productivity score.
- Не делать классификацию приложений.
- Не читать старые логи.
- Не чинить crash recovery.
- Не писать каждое нажатие отдельной строкой.
- Не писать конкретные клавиши.
- Не писать movement mouse events.
- Не добавлять UI.
- Не добавлять отдельный database/storage.
- Не добавлять сетевую синхронизацию.
