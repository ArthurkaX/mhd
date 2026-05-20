# Code Review — mhd-daemon: Modularity & Architecture

## Контекст цели

Цель: **минималистичный демон** — принял конфиг, установил хуки, выполняет задания.
Всё остальное — независимые модули через интерфейсы.

---

## Что уже хорошо ✅

### Слоистая архитектура в целом верная
```
main.rs → app.rs → hook.rs → worker.rs → monitor.rs / trigger
                 ↘ tray.rs (опционально)
                 ↘ osd.rs (опционально)
```
Граница между "ядром" и "UI" (`tray`, `osd`, `about`, `config_editor`) намечена правильно.

### `AppHandle` как публичный интерфейс ядра
`App::handle()` возвращает `AppHandle` — cloneable, thread-safe дескриптор. Это правильный паттерн: ядро не знает, кто его использует (tray, IPC, тесты).

### `config.rs` — чистая, без побочных эффектов
Разбор и валидация TOML полностью изолированы. `AppConfig::parse()` принимает `&str` и возвращает `Result` — легко тестировать без файловой системы.

### `monitor.rs` — хорошая инкапсуляция Win32
`Dxva2` загружается по месту, публичный API — чистые функции `adjust_brightness`, `set_vcp_feature` и т.п. Зависимость на Windows хорошо локализована.

### `trigger.rs` — отличный модуль
Полностью платформонезависимая логика (кроме `get_pressed_modifiers`). Хорошее покрытие тестами. Чёткий тип `Trigger` для HashMap-ключей.

---

## Критические проблемы 🔴

### 1. `hook.rs` — действия выполняются прямо внутри хук-коллбека

**Файл:** [hook.rs](file:///C:/Workspace/Active/mhd/mhd-daemon/src/hook.rs#L216-L236)

```rust
// В keyboard_hook_proc:
Action::SwitchScheme { target_scheme } => {
    let mut config = state.handle.config.lock().unwrap();  // ← мьютекс в хук-коллбеке!
    config.switch_scheme(&target);
}
Action::Quit => {
    state.handle.shutdown();
    signal_tray_to_quit();                // ← FindWindowW в хук-коллбеке!
}
```

**Проблема:** Низкоуровневые хук-коллбеки (WH_KEYBOARD_LL) вызываются в чужих потоках с жёстким таймаутом (~200 мс). Блокировка мьютекса или дополнительные Win32-вызовы могут привести к:
- Зависанию всей системы ввода
- Дедлоку, если другой поток держит `config.lock()` (например, `reload_config`)

**Решение:** Вся логика после сопоставления триггера должна уходить в `tx.send()`. `SwitchScheme` и `Quit` — тоже сообщения воркеру.

```rust
// Вместо встроенной обработки:
let _ = state.tx.send(ActionMessage::Execute(action.clone()));
// Воркер сам разберётся с SwitchScheme и Quit
```

### 2. `hook.rs` — дублирование логики keyboard/mouse (≈80 строк × 2)

**Файл:** [hook.rs L155-L243](file:///C:/Workspace/Active/mhd/mhd-daemon/src/hook.rs#L155-L243) и [L246-L338](file:///C:/Workspace/Active/mhd/mhd-daemon/src/hook.rs#L246-L338)

Блоки `keyboard_hook_proc` и `mouse_hook_proc` содержат идентичную логику:
- Проверка режима записи (`recording_window`)
- Поиск триггера в конфиге
- `match &action { SwitchScheme => ..., Quit => ..., other => send }`
- Добавление в `swallowed_*`

Нужна общая функция:
```rust
fn dispatch_trigger(state: &HookState, trigger: Trigger) -> bool {
    // проверка recording, lookup, dispatch, swallow
    // возвращает: нужно ли проглотить событие
}
```

### 3. `config.rs` — знает о конкретных типах `Action`

**Файл:** [config.rs L90-L131](file:///C:/Workspace/Active/mhd/mhd-daemon/src/config.rs#L90-L131)

```rust
"replace_key" => Action::new_replace_key(keys)?,
"run_ps"      => Action::new_run_ps(command)?,
"set_brightness" => Action::new_set_brightness(value)?,
// ...
```

`config.rs` жёстко перечисляет все действия и знает их поля (`keys`, `command`, `code`, `value`). Добавить новое действие = изменить `config.rs` + `action.rs` + `worker.rs`. Три файла.

**Решение:** Делегировать разбор самому `Action`:
```rust
// в action.rs
impl Action {
    pub fn from_raw(action: &str, raw: &RawBinding) -> Result<Self, String> {
        match action {
            "replace_key" => Self::new_replace_key(raw.keys.as_deref().ok_or("...")?),
            // ...
        }
    }
}
```
Тогда `config.rs` вызывает одну строку: `Action::from_raw(&raw_b.action, &raw_b)?`.

### 4. `main.rs` — содержит бизнес-логику (не место для неё)

**Файл:** [main.rs L26-L51](file:///C:/Workspace/Active/mhd/mhd-daemon/src/main.rs#L26-L51)

`resolve_config_path()`, `home_dir()`, `create_example_config()` и константа `EXAMPLE_CONFIG` живут в `main.rs`. Это затрудняет тестирование и переиспользование.

Должно быть в `config.rs` или отдельном `config_path.rs`.

---

## Значимые замечания 🟡

### 5. `AppHandle` — слишком широкий интерфейс

**Файл:** [app.rs L30-L41](file:///C:/Workspace/Active/mhd/mhd-daemon/src/app.rs#L30-L41)

```rust
pub struct AppHandle {
    pub(crate) running: Arc<AtomicBool>,
    pub(crate) config: Arc<Mutex<AppConfig>>,   // ← весь конфиг
    pub(crate) config_path: PathBuf,
    pub(crate) hook_thread_id: Arc<AtomicU32>,
    pub(crate) quiet: bool,
    pub(crate) theme: Arc<Mutex<NativeTheme>>,
    pub(crate) recording_window: Arc<Mutex<Option<SendHwnd>>>,  // ← детали config_editor
    osd: OsdHandle,
}
```

`recording_window` — это деталь `config_editor`, она не должна жить в `AppHandle`.
`config` напрямую через `Arc<Mutex<>>` — любой модуль может вызвать `config.lock().unwrap()` прямо в хук-коллбеке (что и происходит).

**Решение:** Узкие интерфейсы:
```rust
pub trait DaemonControl: Send + Sync {
    fn shutdown(&self);
    fn reload_config(&self) -> Result<(), String>;
    fn active_scheme(&self) -> String;
    fn switch_scheme(&self, name: &str) -> bool;
}
```
`hook.rs` работает только с `&dyn DaemonControl`, не видит `Arc<Mutex<AppConfig>>` напрямую.

### 6. `worker.rs` — мёртвые варианты в `ActionMessage`

**Файл:** [worker.rs L13-L19](file:///C:/Workspace/Active/mhd/mhd-daemon/src/worker.rs#L13-L19)

```rust
pub enum ActionMessage {
    Execute(Action),
    #[allow(dead_code)]
    SwitchScheme(String),   // ← помечен dead_code, обрабатывается в хук-коллбеке
    #[allow(dead_code)]
    Quit,                   // ← то же самое
}
```

Это симптом проблемы #1: `SwitchScheme` и `Quit` должны идти через воркер, а не обрабатываться в хук-коллбеке. Тогда `dead_code` исчезнет сам.

### 7. `osd.rs` — 637 строк, смешивает OSD-логику и GDI-рендеринг

**Файл:** [osd.rs](file:///C:/Workspace/Active/mhd/mhd-daemon/src/osd.rs)

`paint_osd()` (240+ строк чистого Win32 GDI) находится в том же файле, что и публичный `OsdHandle`. Логично разделить:
- `osd.rs` — публичный `OsdHandle`, команды, тред
- `osd_painter.rs` — `paint_osd()`, `draw_rounded_rect()`, `fix_gdi_alpha()`

### 8. `tray.rs` — глобальный `AtomicPtr<TrayState>` вместо closure

**Файл:** [tray.rs L48](file:///C:/Workspace/Active/mhd/mhd-daemon/src/tray.rs#L48)

```rust
static STATE: AtomicPtr<TrayState> = AtomicPtr::new(ptr::null_mut());
```

Использование leaked Box + AtomicPtr — паттерн для Win32 WndProc, технически оправданный. Но это единственный экземпляр — можно убрать `AtomicPtr` в пользу `OnceLock<Box<TrayState>>` (он безопаснее).

### 9. `trigger.rs` — `get_pressed_modifiers` зависит от Win32

**Файл:** [trigger.rs L262-L285](file:///C:/Workspace/Active/mhd/mhd-daemon/src/trigger.rs#L262-L285)

В файле, который иначе полностью платформонезависим (включая тесты), есть одна функция с `GetAsyncKeyState`. Лучше вынести в `hook.rs` или `platform.rs`, чтобы `trigger.rs` был полностью тестируем без Win32.

---

## Мелкие замечания 🟢

### 10. Дублирование `vk_to_name` / `vk_to_string`
`action.rs` имеет [`vk_to_name`](file:///C:/Workspace/Active/mhd/mhd-daemon/src/action.rs#L158), `trigger.rs` имеет [`vk_to_string`](file:///C:/Workspace/Active/mhd/mhd-daemon/src/trigger.rs#L315) — разные имена, частично разные маппинги. Должна быть одна каноническая функция в `trigger.rs`.

### 11. `monitor.rs` — `std::mem::forget(monitors)` утечка
```rust
std::mem::forget(monitors); // leak -- OS owns the structs
```
Комментарий поясняет ситуацию, но есть риск: если `GetPhysicalMonitorsFromHMONITOR` вернёт `true` с `count=0`, мы всё равно дойдём до `forget`. Нужна явная проверка.

### 12. `Cargo.toml` — нет `[profile.release]`
Для системного демона стоит добавить:
```toml
[profile.release]
opt-level = 3
lto = true
strip = true
```

---

## Предлагаемая целевая структура

```
mhd-daemon/src/
├── main.rs              ← только CLI-аргументы + запуск App
├── app.rs               ← минималистичный демон: конфиг → хуки → воркер
├── config/
│   ├── mod.rs           ← AppConfig (parse, switch_scheme, lookup)
│   ├── path.rs          ← resolve_config_path(), create_example_config()
│   └── raw.rs           ← RawConfig, RawBinding (приватные serde-структуры)
├── action.rs            ← Action enum + from_raw() + describe()
├── trigger.rs           ← Trigger, Modifiers, parse_trigger() (без Win32)
├── hook.rs              ← Win32 хуки + dispatch_trigger() (только Send)
├── worker.rs            ← ActionWorker (выполняет Actions)
├── monitor.rs           ← DDC/CI через dxva2.dll
├── platform.rs          ← get_pressed_modifiers(), SendInput и другой Win32-glue
├── osd/
│   ├── mod.rs           ← OsdHandle, start_osd()
│   └── painter.rs       ← paint_osd(), draw_rounded_rect()
├── tray.rs              ← системный трей (опциональный UI)
├── native_theme.rs      ← NativeTheme + load_theme()
├── about.rs             ← диалог "О программе"
└── config_editor.rs     ← редактор конфига (опциональный UI)
```

### Приоритет изменений

| # | Проблема | Приоритет | Сложность |
|---|----------|-----------|-----------|
| 1 | Действия в хук-коллбеке | 🔴 Критично | Средняя |
| 2 | Дублирование hook kbd/mouse | 🔴 Критично | Малая |
| 3 | config.rs знает об Action | 🟡 Важно | Малая |
| 4 | Бизнес-логика в main.rs | 🟡 Важно | Малая |
| 5 | AppHandle — узкие трейты | 🟡 Важно | Средняя |
| 6 | Мёртвые варианты ActionMessage | 🟢 Попутно с #1 | Малая |
| 7 | Разбить osd.rs | 🟢 Удобство | Малая |
| 9 | get_pressed_modifiers в trigger.rs | 🟢 Чистота | Малая |
| 10 | Дублирование vk_to_* | 🟢 Чистота | Малая |
