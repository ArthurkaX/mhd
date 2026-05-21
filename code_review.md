# Code Review — mhd-daemon

> Цель: оценить код с точки зрения **модульности**, **скорости работы**, **DRY** и **KISS**.
> Ключевой принцип: держать в памяти лёгкий демон, а «тяжёлые» модули подгружать по требованию.

---

## 1. Общая архитектура — что хорошо ✅

| Аспект | Оценка |
|---|---|
| Разделение hot-path (hook) и heavy-path (worker thread) | Отлично |
| `OnceLock` для hook-state — lock-free read в hot-path | Правильно |
| `mpsc::channel` между hook → worker — нет блокировок в callback | Правильно |
| `AppHandle` как тонкий Arc-wrapper вместо передачи App напрямую | Хорошо |
| `DaemonControl` trait для тестируемости | Хорошо |
| Динамическая загрузка `dxva2.dll` (DDC/CI только когда нужно) | В духе lazy-load |

---

## 2. Критические замечания

### 2.1. `Dxva2::load()` — повторная загрузка DLL на каждый вызов

**Файл:** [monitor.rs](file:///C:/Workspace/Active/mhd/mhd-daemon/src/monitor.rs#L56-L93)

```rust
// Сейчас — каждая публичная функция делает:
pub fn adjust_brightness(delta: i32) -> Result<(), String> {
    let (dxva2, handle, _) = cursor_monitor_raw()?;  // <- Dxva2::load() внутри!
    ...
}
```

`Dxva2::load()` вызывается при каждом нажатии клавиши яркости:
1. `LoadLibraryA("dxva2.dll")` — системный вызов
2. 8× `GetProcAddress` — ещё 8 системных вызовов
3. `GetPhysicalMonitorsFromHMONITOR` — ещё DDC/CI round-trip

**Рекомендация:** кэшировать `Dxva2` в `OnceLock` или `LazyLock`. Физические мониторы меняются редко — их тоже можно кэшировать с инвалидацией по `WM_DISPLAYCHANGE`.

```rust
// Предлагаемое решение:
static DXVA2: LazyLock<Result<Dxva2, String>> = LazyLock::new(|| Dxva2::load());

pub fn adjust_brightness(delta: i32) -> Result<(), String> {
    let dxva2 = DXVA2.as_ref().map_err(|e| e.clone())?;
    ...
}
```

> [!CAUTION]
> Это **самая горячая** точка производительности — DDC/CI вызовы занимают десятки мс на некоторых мониторах. Повторная загрузка DLL добавляет лишние системные вызовы.

---

### 2.2. `Box::leak` для `HookState` — намеренная утечка памяти

**Файл:** [hook.rs](file:///C:/Workspace/Active/mhd/mhd-daemon/src/hook.rs#L94-L101)

```rust
let state_ref: &'static HookState = Box::leak(state);
let _ = HOOK_STATE.set(state_ref);
```

Комментарий объясняет причину, но при `reload_config` или будущей поддержке перезапуска хуков — `OnceLock` не позволит заменить состояние. Если потребуется поддержка хот-релоад хуков, нужна `ArcSwap` или двойная буферизация.

**Сейчас:** допустимо для однократного запуска, но стоит явно задокументировать ограничение.

---

### 2.3. `toggle_topmost_pin()` — находится в `worker.rs`

**Файл:** [worker.rs](file:///C:/Workspace/Active/mhd/mhd-daemon/src/worker.rs#L184-L249)

Функция `toggle_topmost_pin` — полноценный UI-модуль (200+ строк, Win32 DWM API), но живёт в `worker.rs`. Это нарушает KISS: worker должен только диспетчировать действия.

**Рекомендация:** вынести в отдельный `topmost.rs`.

---

### 2.4. Дублирование в `app.rs` — `AppHandle` дублирует методы `DaemonControl`

**Файл:** [app.rs](file:///C:/Workspace/Active/mhd/mhd-daemon/src/app.rs#L135-L151)

```rust
// Блок impl AppHandle повторяет КАЖДЫЙ метод из DaemonControl:
impl AppHandle {
    pub fn theme(&self) -> NativeTheme { DaemonControl::theme(self) }
    pub fn status(&self) -> bool { DaemonControl::status(self) }
    pub fn reload_config(&self) -> Result<(), String> { DaemonControl::reload_config(self) }
    pub fn shutdown(&self) { DaemonControl::shutdown(self) }
    pub fn switch_scheme(&self, name: &str) -> bool { DaemonControl::switch_scheme(self, name) }
}
```

Это чистый DRY-нарушитель. Либо:
- Использовать методы трейта напрямую (`handle.reload_config()` работает и через трейт, если трейт в области видимости), или
- Убрать дублирующий `impl AppHandle`, добавить `use crate::app::DaemonControl` там где нужно.

---

### 2.5. `show_menu()` в `tray.rs` — повторяющийся паттерн InsertMenuW

**Файл:** [tray.rs](file:///C:/Workspace/Active/mhd/mhd-daemon/src/tray.rs#L101-L197)

```rust
// Один и тот же паттерн повторяется 7 раз:
let text: Vec<u16> = "Edit Config\0".encode_utf16().collect();
let _ = InsertMenuW(menu, pos, MF_BYPOSITION | MF_STRING, CMD_X,
    PCWSTR::from_raw(text.as_ptr()));
```

**Рекомендация — выделить хелпер:**

```rust
unsafe fn insert_menu_item(menu: HMENU, pos: u32, cmd: usize, label: &str) {
    let wide: Vec<u16> = label.encode_utf16().chain([0]).collect();
    let _ = InsertMenuW(menu, pos, MF_BYPOSITION | MF_STRING, cmd,
        PCWSTR::from_raw(wide.as_ptr()));
}
```

---

### 2.6. `parse_trigger` и `parse_keys` — дублирование логики

**Файл:** [trigger.rs](file:///C:/Workspace/Active/mhd/mhd-daemon/src/trigger.rs#L61-L128)

Обе функции идентичны на 90% — разница только в том, что `parse_trigger` требует обязательный non-modifier ключ, а `parse_keys` — нет.

**Рекомендация:**

```rust
// Общая внутренняя функция:
fn parse_combo_inner(s: &str) -> Result<(Modifiers, Option<PhysicalKey>), String> { ... }

pub fn parse_trigger(s: &str) -> Result<ParsedTrigger, String> {
    let (mods, key) = parse_combo_inner(s)?;
    let key = key.ok_or_else(|| format!("no non-modifier key in trigger: '{s}'"))?;
    Ok(ParsedTrigger { trigger: Trigger { modifiers: mods, key }, original: s.trim().to_string() })
}

pub fn parse_keys(s: &str) -> Result<KeyCombo, String> {
    let (mods, key) = parse_combo_inner(s)?;
    Ok(KeyCombo { modifiers: mods, key })
}
```

---

### 2.7. `signal_tray_to_quit()` — жёстко зашитые строки

**Файл:** [hook.rs](file:///C:/Workspace/Active/mhd/mhd-daemon/src/hook.rs#L142-L157)

```rust
let class: Vec<u16> = "mhdTrayClass\0".encode_utf16().collect();
let title: Vec<u16> = "mhd-tray\0".encode_utf16().collect();
```

Те же строки определены в [tray.rs](file:///C:/Workspace/Active/mhd/mhd-daemon/src/tray.rs#L303-L304). Это DRY-нарушение — при переименовании одна сторона останется несинхронизированной.

**Рекомендация:** вынести в общий модуль `constants.rs` или в `tray.rs` как `pub const`.

---

### 2.8. `volume_mixer` и `monitor_panel` — глобальное состояние через модульные функции

В `main.rs`:
```rust
volume_mixer::set_theme(handle.theme());
monitor_panel::set_theme(handle.theme());
```

Эти модули используют внутренние `static`-переменные с `Mutex`. Это неявная зависимость — нельзя создать две независимые копии. Если когда-либо понадобится тест или второй экземпляр — сложно.

**Для daemon-цели (один экземпляр):** приемлемо.
**Для тестируемости:** лучше передавать конфигурацию явно при вызове `show()`.

---

## 3. Стратегия Lazy-Load модулей

> [!IMPORTANT]
> Цель: держать демон лёгким и подгружать тяжёлые UI-модули только по запросу.

### Текущее состояние:

| Модуль | Размер | Загрузка | Статус |
|---|---|---|---|
| `hook.rs` | ~14 KB | Всегда (hot path) | ✅ Нормально |
| `worker.rs` | ~10 KB | Всегда (1 поток) | ✅ Нормально |
| `osd/` | ~18 KB | Всегда (1 поток) | ⚠️ Можно lazy |
| `monitor.rs` | ~18 KB | По требованию (dxva2.dll lazy) | ⚠️ DLL загружается повторно |
| `monitor_panel.rs` | **53 KB** | Всегда через static | ❌ Тяжёлый, нужен lazy |
| `volume_mixer.rs` | **37 KB** | Всегда через static | ❌ Тяжёлый, нужен lazy |
| `config_editor.rs` | **119 KB** | По требованию (только из tray) | ✅ Хорошо |
| `about.rs` | ~11 KB | По требованию | ✅ Хорошо |
| `tray.rs` | ~12 KB | По требованию (--no-tray пропускает) | ✅ Хорошо |

### Рекомендации по lazy-load:

**`monitor_panel` и `volume_mixer`** — самые тяжёлые модули (53 KB + 37 KB кода). Сейчас они инициализируют глобальный `static` при первом вызове `set_theme()`, что происходит всегда при старте. Рекомендуется:

```rust
// Вместо вызова set_theme при старте — передавать theme в show():
pub fn show(theme: NativeTheme) { ... }
// Убрать set_theme() как отдельный вызов

// В main.rs убрать:
// volume_mixer::set_theme(handle.theme());   // <- убрать
// monitor_panel::set_theme(handle.theme()); // <- убрать

// Тогда модули не трогаются до первого нажатия горячей клавиши
```

**OSD** — запускает поток всегда. Можно сделать lazy: создавать OSD-поток только при первом вызове `show_brightness()`.

---

## 4. Производительность в hot-path

### Hot-path: `keyboard_hook_proc` → `dispatch_trigger` → lookup

```
GetMessage → hook callback → OnceLock::get() [lock-free] → swallowed_keys.lock() → lookup_trigger → config.lock()
```

**Хорошо:**
- `OnceLock::get()` — атомарное чтение, без lock
- `HashMap<Trigger, usize>` lookup — O(1)

**Потенциальная проблема:**
- `swallowed_keys.lock()` — `Mutex` в hot-path. При высокой частоте событий (typist) это может создавать давление. Можно заменить на `AtomicU64` с битовыми флагами для клавиш (256 VK влазит в 4 u64) — lock-free.
- `config.lock()` в `lookup_trigger` через `AppHandle` — ещё один Mutex на каждое нажатие.

**Рекомендация для swallowed_keys:**
```rust
// Текущий код (с lock):
swallowed_keys: Mutex<HashSet<u32>>,

// Предлагаемая замена (lock-free для клавиш 0..=255):
swallowed_keys: [AtomicBool; 256],
swallowed_mouse: AtomicU8, // битовые флаги для mouse buttons 1-3
```

---

## 5. Мелкие замечания (KISS / стиль)

### 5.1. `worker.rs` — избыточная переменная

```rust
// worker.rs:54
let action_to_execute = msg;  // <- бесполезное переименование
```

### 5.2. `monitor.rs` — `enumerate_cursor_monitor` можно упростить

```rust
// Сейчас:
match get_physical_monitors_for_hmon(&dxva2, hmon) {
    Ok((handle, name)) => { Ok(vec![PhysicalMonitorInfo { handle, name }]) }
    Err(e) => Err(e),
}
// Лучше:
get_physical_monitors_for_hmon(&dxva2, hmon)
    .map(|(handle, name)| vec![PhysicalMonitorInfo { handle, name }])
```

### 5.3. `main.rs` — три `set_theme` подряд

```rust
osd_handle.set_theme(handle.theme());
volume_mixer::set_theme(handle.theme());
monitor_panel::set_theme(handle.theme());
```

Если убрать инициализацию `set_theme` при старте (см. п. 3), эти строки уйдут, и `main.rs` станет чище.

### 5.4. `action.rs` — `brightness_up` / `brightness_down` дублируют логику

```rust
"brightness_up" => {
    let value = fields.value.and_then(|v| v.parse::<u32>().ok()).unwrap_or(5);
    if value == 0 { return Err(...); }
    Ok(Action::BrightnessUp { value })
}
"brightness_down" => {
    // Точно такой же код
}
```

**Рекомендация:** выделить `parse_brightness_step(fields) -> Result<u32, String>`.

### 5.5. `hook.rs` — две пустые строки в конце файла (minor)

### 5.6. `Cargo.toml` — `lazy_static` vs `std::sync::LazyLock`

```toml
lazy_static = "1.4"
```

В `hook.rs` уже используется `std::sync::LazyLock` (стабилизирован в Rust 1.80). `lazy_static` в `Cargo.toml` — скорее всего остаточная зависимость. Проверить, используется ли вообще.

---

## 6. Сводная таблица приоритетов

| # | Проблема | Приоритет | Файл |
|---|---|---|---|
| 1 | `Dxva2::load()` при каждом вызове | 🔴 HIGH | monitor.rs |
| 2 | `monitor_panel`/`volume_mixer` инициализация при старте | 🔴 HIGH | main.rs |
| 3 | `toggle_topmost_pin()` в worker.rs | 🟡 MED | worker.rs |
| 4 | Дублирование методов `AppHandle` vs `DaemonControl` | 🟡 MED | app.rs |
| 5 | `parse_trigger` / `parse_keys` дублируют логику | 🟡 MED | trigger.rs |
| 6 | Жёстко зашитые строки класса/окна tray | 🟡 MED | hook.rs / tray.rs |
| 7 | `swallowed_keys` Mutex в hot-path | 🟢 LOW | hook.rs |
| 8 | `lazy_static` — неиспользуемая зависимость | 🟢 LOW | Cargo.toml |
| 9 | Мелкие упрощения (`let action_to_execute = msg`, enumerate_cursor_monitor) | 🟢 LOW | monitor.rs / worker.rs |
| 10 | `brightness_up`/`brightness_down` дублирование | 🟢 LOW | action.rs |
