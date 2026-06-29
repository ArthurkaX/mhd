pub mod editor;
pub mod editor_binding_popup;
pub mod editor_control;
pub mod editor_hittest;
pub mod editor_key_combo;
pub mod editor_layout;
pub mod editor_paint;
pub mod editor_provider_models_popup;
pub mod editor_provider_popup;
pub mod editor_search_dropdown;
pub mod editor_state;
pub mod editor_theme;
pub mod path;
pub mod raw;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use self::raw::RawConfig;
use crate::action::Action;
#[cfg(feature = "blackbox")]
use crate::blackbox::BlackboxConfig;
use crate::config::path::home_dir;
use crate::overlays::keycast::{KeycastConfig, KeycastPosition};
use crate::overlays::note::QuickNoteConfig;
use crate::trigger::parse_trigger;
use llm_proxy;

fn default_notes_dir() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("mhd")
        .join("notes")
}

fn default_draw_dir() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("mhd")
        .join("screenshots")
}

/// One upstream provider (OpenAI-compatible).
/// API keys are stored separately in secrets.json.
#[derive(Debug, Clone, PartialEq)]
pub struct Provider {
    pub name: String,
    pub endpoint: String,
}

/// One selectable alternative model in the LLM proxy selector.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmModel {
    pub provider: String,
    pub id: String,
    pub display_name: String,
    pub tags: Vec<String>,
}

/// Validated LLM proxy config (loaded from JSON files).
#[derive(Debug, Clone)]
pub struct LlmProxyConfig {
    pub enabled: bool,
    pub port: u16,
    pub log_level: String,
    /// Bind IP address (e.g. "127.0.0.1").
    pub bind_address: String,
    pub providers: Vec<Provider>,
    /// Optional Anthropic API key for native passthrough (usually empty — OAuth
    /// from Claude Code is forwarded instead).
    pub anthropic_key: String,
    /// Bearer key for the upstream gateway (from secrets.json).
    pub upstream_key: String,
    /// Default routing target per tier: "native" or an upstream model id.
    pub opus: String,
    pub sonnet: String,
    pub haiku: String,
    pub fable: String,
    /// Shared pool of alternative models offered for every tier.
    pub models: Vec<LlmModel>,
    /// Opus downgrade when no thinking.
    pub opus_downgrade_enabled: bool,
    /// Sonnet downgrade when no thinking.
    pub sonnet_downgrade_enabled: bool,
    /// Selected model for vision screenshot feature.
    pub vision_model: Option<llm_proxy::config::ModelRef>,
    /// Enable llmtrim-powered request compression.
    pub trim_enabled: bool,
    /// Enable replay-corpus capture: store full pre-trim request bodies in
    /// proxy.db so a candidate trim preset can be backtested against real traffic.
    pub corpus_capture: bool,
    /// Trim engine: "llmtrim" (default) or "native".
    pub trim_engine: String,
    /// Max Unicode chars per tool description (native engine).
    pub trim_tool_desc_chars: usize,
    /// Max chars for the head of a tool_result block (native engine).
    pub trim_toolresult_head: usize,
    /// Max chars for the tail of a tool_result block (native engine).
    pub trim_toolresult_tail: usize,
}

impl Default for LlmProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 3456,
            log_level: "none".to_string(),
            providers: Vec::new(),
            bind_address: "127.0.0.1".to_string(),
            anthropic_key: String::new(),
            upstream_key: String::new(),
            opus: "native".to_string(),
            sonnet: "native".to_string(),
            haiku: "native".to_string(),
            fable: "native".to_string(),
            models: Vec::new(),
            opus_downgrade_enabled: false,
            sonnet_downgrade_enabled: false,
            vision_model: None,
            trim_enabled: false,
            corpus_capture: false,
            trim_engine: "llmtrim".to_string(),
            trim_tool_desc_chars: 150,
            trim_toolresult_head: 3000,
            trim_toolresult_tail: 1000,
        }
    }
}

/// A validated binding.
#[derive(Debug, Clone)]
pub struct Binding {
    pub trigger: crate::trigger::Trigger,
    pub trigger_name: String,
    pub action: Action,
    pub scheme: String,
}

/// The fully validated application configuration.
#[derive(Debug)]
pub struct AppConfig {
    active_scheme: String,
    bindings: Vec<Binding>,
    /// All scheme names that exist in the config (for validation).
    scheme_names: HashSet<String>,
    /// Trigger map for the active scheme: Trigger -> index into bindings.
    trigger_map: HashMap<crate::trigger::Trigger, usize>,
    /// The theme name from config.
    pub theme: Option<String>,
    /// Volume adjustment step for `media_volume_up` / `media_volume_down`.
    pub volume_step: u32,
    /// Autostart at user logon (via scheduled task).
    #[allow(dead_code)]
    pub autostart: bool,
    /// Behavioural logger config.
    #[cfg(feature = "blackbox")]
    pub blackbox: BlackboxConfig,
    /// Quick Note config.
    pub quicknote: QuickNoteConfig,
    /// Quick Draw save directory.
    pub draw_dir: PathBuf,
    /// KeyCast overlay config.
    pub keycast: KeycastConfig,
    /// Ordered list of power plan names for rotation.
    pub power_plans: Vec<String>,
    /// LLM proxy integration config.
    pub llm_proxy: LlmProxyConfig,
}

impl AppConfig {
    /// Parse and validate config from a TOML string.
    pub fn parse(content: &str, _path: &Path) -> Result<Self, String> {
        let raw: RawConfig =
            toml::from_str(content).map_err(|e| format!("config parse error: {e}"))?;

        let active_scheme = raw.active_scheme.unwrap_or_else(|| "default".to_string());
        let theme = raw.theme;
        let mut bindings = Vec::new();
        let mut scheme_names = HashSet::new();
        scheme_names.insert("default".to_string());

        // Collect all scheme names first for cross-validation
        for raw_b in &raw.binding {
            if let Some(ref s) = raw_b.scheme {
                scheme_names.insert(s.clone());
            }
        }

        for raw_b in &raw.binding {
            let scheme = raw_b
                .scheme
                .clone()
                .unwrap_or_else(|| "default".to_string());

            // Parse trigger — skip invalid triggers with a warning
            let parsed_trigger = match parse_trigger(&raw_b.trigger) {
                Ok(pt) => pt,
                Err(e) => {
                    eprintln!("mhd: warning — skipping binding '{}': {e}", raw_b.trigger);
                    continue;
                }
            };

            // Validate and create action — skip invalid actions with a warning
            let action = match crate::action::Action::from_raw(
                &raw_b.action,
                crate::action::ActionRawFields {
                    keys: raw_b.keys.as_deref(),
                    command: raw_b.command.as_deref(),
                    path: raw_b.path.as_deref(),
                    target_scheme: raw_b.target_scheme.as_deref(),
                    value: raw_b.value.as_deref(),
                    code: raw_b.code.as_deref(),
                    target: raw_b.target.as_deref(),
                    preset: raw_b.preset.as_deref(),
                },
            ) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("mhd: warning — skipping binding '{}': {e}", raw_b.trigger);
                    continue;
                }
            };

            bindings.push(Binding {
                trigger: parsed_trigger.trigger,
                trigger_name: parsed_trigger.original,
                action,
                scheme,
            });
        }

        // Validate that all switch_scheme targets exist
        for binding in &bindings {
            if let Action::SwitchScheme { target_scheme } = &binding.action
                && !scheme_names.contains(target_scheme)
            {
                return Err(format!(
                    "switch_scheme target '{}' does not exist in config",
                    target_scheme
                ));
            }
        }

        // Warn about duplicate triggers within same scheme (non-fatal)
        let mut seen_triggers: HashMap<(String, crate::trigger::Trigger), ()> = HashMap::new();
        for binding in &bindings {
            let key = (binding.scheme.clone(), binding.trigger);
            if seen_triggers.contains_key(&key) {
                eprintln!(
                    "mhd: warning — duplicate trigger '{}' in scheme '{}', last wins",
                    binding.trigger_name, binding.scheme
                );
            }
            seen_triggers.insert(key, ());
        }

        // Build trigger map for the active scheme
        let trigger_map = Self::build_trigger_map(&bindings, &active_scheme);

        Ok(AppConfig {
            active_scheme,
            bindings,
            scheme_names,
            trigger_map,
            theme,
            volume_step: raw.volume_step.unwrap_or(1),
            #[cfg(feature = "blackbox")]
            blackbox: BlackboxConfig {
                enabled: raw
                    .blackbox
                    .as_ref()
                    .and_then(|b| b.enabled)
                    .unwrap_or(false),
                idle_seconds: raw
                    .blackbox
                    .as_ref()
                    .and_then(|b| b.idle_seconds)
                    .unwrap_or(300),
                track_locks: raw
                    .blackbox
                    .as_ref()
                    .and_then(|b| b.track_locks)
                    .unwrap_or(true),
                track_suspend: raw
                    .blackbox
                    .as_ref()
                    .and_then(|b| b.track_suspend)
                    .unwrap_or(true),
                window_title_filter: raw
                    .blackbox
                    .as_ref()
                    .and_then(|b| b.window_title_filter.clone())
                    .unwrap_or_default(),
            },
            quicknote: QuickNoteConfig {
                enabled: raw
                    .quicknote
                    .as_ref()
                    .and_then(|q| q.enabled)
                    .unwrap_or(true),
                notes_dir: raw
                    .quicknote
                    .as_ref()
                    .and_then(|q| q.notes_dir.as_ref())
                    .map(PathBuf::from)
                    .unwrap_or_else(default_notes_dir),
            },
            draw_dir: raw
                .quickdraw
                .as_ref()
                .and_then(|q| q.draw_dir.as_ref())
                .map(PathBuf::from)
                .unwrap_or_else(default_draw_dir),
            keycast: parse_keycast_config(raw.keycast.as_ref()),
            autostart: raw.autostart.unwrap_or(false),
            power_plans: raw.power_plans,
            // LLM proxy config is now stored in JSON files under the proxy's
            // config directory. The old `[llm_proxy]` TOML section is migrated
            // automatically on first access.
            llm_proxy: Self::load_llm_proxy_from_json(content),
        })
    }

    // ── LLM proxy JSON loading ──────────────────────────────────────────────

    /// Load LLM proxy config from JSON files. Falls back to defaults if files
    /// don't exist. Migrates old `[llm_proxy]` TOML section on first run.
    fn load_llm_proxy_from_json(toml_content: &str) -> LlmProxyConfig {
        // Migration: old TOML `[llm_proxy]` → JSON files
        Self::maybe_migrate_llm_proxy_toml(toml_content);

        // Load from JSON files
        let settings = llm_proxy::config::load_settings().ok();
        let secrets = llm_proxy::config::load_secrets().ok();
        let providers = llm_proxy::config::load_providers().ok();
        let models = llm_proxy::config::load_models().ok();

        LlmProxyConfig {
            enabled: settings.as_ref().map(|s| s.enabled).unwrap_or(false),
            port: settings.as_ref().map(|s| s.port).unwrap_or(3456),
            log_level: settings
                .as_ref()
                .map(|s| s.log_level.clone())
                .unwrap_or_else(|| "none".to_string()),
            providers: providers
                .unwrap_or_default()
                .into_iter()
                .map(|p| Provider {
                    name: p.name,
                    endpoint: p.endpoint,
                })
                .collect(),
            anthropic_key: secrets
                .as_ref()
                .map(|s| s.anthropic_key.clone())
                .unwrap_or_default(),
            upstream_key: secrets
                .as_ref()
                .map(|s| s.upstream_key.clone())
                .unwrap_or_default(),
            opus: settings
                .as_ref()
                .map(|s| s.opus_target.clone())
                .unwrap_or_else(|| "native".to_string()),
            sonnet: settings
                .as_ref()
                .map(|s| s.sonnet_target.clone())
                .unwrap_or_else(|| "native".to_string()),
            haiku: settings
                .as_ref()
                .map(|s| s.haiku_target.clone())
                .unwrap_or_else(|| "native".to_string()),
            fable: settings
                .as_ref()
                .map(|s| s.fable_target.clone())
                .unwrap_or_else(|| "native".to_string()),
            models: models
                .unwrap_or_default()
                .into_iter()
                .map(|m| LlmModel {
                    provider: m.provider,
                    id: m.id,
                    display_name: m.display_name,
                    tags: m.tags,
                })
                .collect(),
            bind_address: settings
                .as_ref()
                .map(|s| s.bind_ip.clone())
                .unwrap_or_else(|| "127.0.0.1".to_string()),
            opus_downgrade_enabled: settings
                .as_ref()
                .map(|s| s.opus_downgrade_enabled)
                .unwrap_or(false),
            sonnet_downgrade_enabled: settings
                .as_ref()
                .map(|s| s.sonnet_downgrade_enabled)
                .unwrap_or(false),
            vision_model: settings.as_ref().and_then(|s| s.vision_model.clone()),
            trim_enabled: settings.as_ref().map(|s| s.trim_enabled).unwrap_or(false),
            corpus_capture: settings.as_ref().map(|s| s.corpus_capture).unwrap_or(false),
            trim_engine: settings
                .as_ref()
                .map(|s| s.trim_engine.clone())
                .unwrap_or_else(|| "llmtrim".to_string()),
            trim_tool_desc_chars: settings
                .as_ref()
                .map(|s| s.trim_tool_desc_chars)
                .unwrap_or(150),
            trim_toolresult_head: settings
                .as_ref()
                .map(|s| s.trim_toolresult_head)
                .unwrap_or(3000),
            trim_toolresult_tail: settings
                .as_ref()
                .map(|s| s.trim_toolresult_tail)
                .unwrap_or(1000),
        }
    }

    /// Migrate old `[llm_proxy]` TOML section to JSON files.
    /// Only runs once; after migration the TOML section is harmless (serde ignores
    /// unknown fields) and the JSON files take over.
    fn maybe_migrate_llm_proxy_toml(content: &str) {
        // Parse the TOML content looking for `[llm_proxy]`. We use the old raw
        // struct directly to avoid duplicating field definitions.
        #[derive(serde::Deserialize)]
        struct OldLlmProxy {
            #[serde(default)]
            enabled: Option<bool>,
            #[serde(default)]
            port: Option<u16>,
            #[serde(default)]
            log_level: Option<String>,
            #[serde(default)]
            provider: Vec<OldProvider>,
            #[serde(default)]
            anthropic_key: Option<String>,
            #[serde(default)]
            api_key: Option<String>,
            #[serde(default)]
            endpoint: Option<String>,
            #[serde(default)]
            opus: Option<String>,
            #[serde(default)]
            sonnet: Option<String>,
            #[serde(default)]
            haiku: Option<String>,
            #[serde(default)]
            fable: Option<String>,
            #[serde(default)]
            model: Vec<OldModel>,
        }

        #[derive(serde::Deserialize)]
        struct OldProvider {
            name: String,
            endpoint: String,
            #[serde(default)]
            api_key: String,
        }

        #[derive(serde::Deserialize)]
        struct OldModel {
            #[serde(default)]
            provider: String,
            id: String,
            #[serde(default)]
            display_name: Option<String>,
            #[serde(default)]
            name: Option<String>,
            #[serde(default)]
            tags: Vec<String>,
        }

        #[derive(serde::Deserialize)]
        struct OldConfig {
            llm_proxy: Option<OldLlmProxy>,
        }

        // Only migrate if the JSON files don't already exist.
        let dir = llm_proxy::config::config_dir();
        if llm_proxy::config::load_settings().is_ok() {
            return; // already migrated
        }

        let old: OldConfig = match toml::from_str(content) {
            Ok(c) => c,
            Err(_) => return,
        };
        let Some(lp) = old.llm_proxy else {
            return;
        };

        // Build settings
        let mut settings = llm_proxy::config::Settings::default();
        settings.enabled = lp.enabled.unwrap_or(false);
        settings.port = lp.port.unwrap_or(3456);
        settings.log_level = lp.log_level.unwrap_or_else(|| "none".to_string());
        let upstream_base_url = lp
            .endpoint
            .clone()
            .or_else(|| lp.provider.first().map(|p| p.endpoint.clone()));
        if let Some(url) = upstream_base_url {
            settings.upstream_base_url = url;
        }
        settings.opus_target = lp.opus.unwrap_or_else(|| "native".to_string());
        settings.sonnet_target = lp.sonnet.unwrap_or_else(|| "native".to_string());
        settings.haiku_target = lp.haiku.unwrap_or_else(|| "native".to_string());
        settings.fable_target = lp.fable.unwrap_or_else(|| "native".to_string());

        // Build secrets
        let mut secrets = llm_proxy::config::Secrets::default();
        if let Some(k) = lp.anthropic_key.filter(|k| !k.is_empty()) {
            secrets.anthropic_key = k;
        }
        // Use first provider's api_key or the deprecated top-level api_key
        let upstream_key = lp
            .provider
            .first()
            .map(|p| p.api_key.clone())
            .or(lp.api_key)
            .unwrap_or_default();
        if !upstream_key.is_empty() {
            secrets.upstream_key = upstream_key;
        }

        // Build providers (name + endpoint only, keys go to secrets)
        let providers: Vec<llm_proxy::config::Provider> =
            if lp.provider.is_empty() && lp.endpoint.is_some() {
                vec![llm_proxy::config::Provider {
                    name: "Default".into(),
                    endpoint: lp.endpoint.unwrap(),
                }]
            } else {
                lp.provider
                    .into_iter()
                    .map(|p| llm_proxy::config::Provider {
                        name: p.name,
                        endpoint: p.endpoint,
                    })
                    .collect()
            };

        // Build models
        let default_provider = providers
            .first()
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let models: Vec<llm_proxy::config::Model> = lp
            .model
            .into_iter()
            .map(|m| {
                let display_name = m.display_name.or(m.name).unwrap_or_else(|| m.id.clone());
                llm_proxy::config::Model {
                    provider: if m.provider.is_empty() {
                        default_provider.clone()
                    } else {
                        m.provider
                    },
                    id: m.id,
                    display_name,
                    tags: m.tags,
                }
            })
            .collect();

        // Persist all files
        let _ = std::fs::create_dir_all(&dir);
        if let Err(e) = llm_proxy::config::save_settings(&settings) {
            eprintln!("mhd: failed to migrate proxy settings: {e}");
            return;
        }
        if let Err(e) = llm_proxy::config::save_secrets(&secrets) {
            eprintln!("mhd: failed to migrate proxy secrets: {e}");
            return;
        }
        if let Err(e) = llm_proxy::config::save_providers(&providers) {
            eprintln!("mhd: failed to migrate proxy providers: {e}");
            return;
        }
        if let Err(e) = llm_proxy::config::save_models(&models) {
            eprintln!("mhd: failed to migrate proxy models: {e}");
            return;
        }

        eprintln!("mhd: migrated [llm_proxy] config to JSON files");
    }

    fn build_trigger_map(
        bindings: &[Binding],
        active_scheme: &str,
    ) -> HashMap<crate::trigger::Trigger, usize> {
        let mut map = HashMap::new();
        for (i, binding) in bindings.iter().enumerate() {
            if binding.scheme == active_scheme {
                if let std::collections::hash_map::Entry::Vacant(e) = map.entry(binding.trigger) {
                    e.insert(i);
                } else {
                    eprintln!(
                        "mhd: warning — duplicate trigger '{}' in scheme '{}', ignoring",
                        binding.trigger_name, active_scheme
                    );
                }
            }
        }
        map
    }

    /// Get the bindings for the currently active scheme.
    pub fn active_bindings(&self) -> Vec<&Binding> {
        self.bindings
            .iter()
            .filter(|b| b.scheme == self.active_scheme)
            .collect()
    }

    /// Look up a trigger in the active scheme.
    pub fn lookup_trigger(&self, trigger: &crate::trigger::Trigger) -> Option<&Binding> {
        self.trigger_map.get(trigger).map(|&i| &self.bindings[i])
    }

    /// Switch the active scheme. Returns false if the scheme doesn't exist.
    pub fn switch_scheme(&mut self, new_scheme: &str) -> bool {
        if !self.scheme_names.contains(new_scheme) {
            return false;
        }
        self.active_scheme = new_scheme.to_string();
        self.trigger_map = Self::build_trigger_map(&self.bindings, &self.active_scheme);
        true
    }

    /// Get the active scheme name.
    pub fn active_scheme(&self) -> &str {
        &self.active_scheme
    }

    /// Get the full bindings list.
    #[allow(dead_code)]
    pub fn bindings(&self) -> &[Binding] {
        &self.bindings
    }

    /// Volume adjustment step.
    pub fn volume_step(&self) -> u32 {
        self.volume_step
    }

    /// Whether autostart is enabled.
    #[allow(dead_code)]
    pub fn autostart(&self) -> bool {
        self.autostart
    }

    /// Blackbox configuration.
    #[cfg(feature = "blackbox")]
    pub fn blackbox(&self) -> &BlackboxConfig {
        &self.blackbox
    }

    pub fn quicknote_config(&self) -> &QuickNoteConfig {
        &self.quicknote
    }

    pub fn draw_dir(&self) -> &PathBuf {
        &self.draw_dir
    }

    pub fn keycast_config(&self) -> &KeycastConfig {
        &self.keycast
    }

    /// LLM proxy configuration.
    pub fn llm_proxy(&self) -> &LlmProxyConfig {
        &self.llm_proxy
    }
}

fn parse_keycast_config(raw: Option<&self::raw::RawKeycast>) -> KeycastConfig {
    let Some(raw) = raw else {
        return KeycastConfig::default();
    };

    let mut cfg = KeycastConfig::default();
    if let Some(position) = raw.position.as_deref().and_then(KeycastPosition::parse) {
        cfg.position = position;
    }
    if let Some(duration_ms) = raw.duration_ms.filter(|v| *v >= 250) {
        cfg.duration_ms = duration_ms;
    }
    if let Some(show_typing) = raw.show_typing {
        cfg.show_typing = show_typing;
    }
    if let Some(width) = raw.typing_width_chars.filter(|v| *v >= 4 && *v <= 80) {
        cfg.typing_width_chars = width;
    }
    if let Some(typing_ms) = raw.typing_duration_ms.filter(|v| *v >= 250) {
        cfg.typing_duration_ms = typing_ms;
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // ── Helpers ───────────────────────────────────────────────────

    fn valid_config() -> &'static str {
        r#"
[[ binding ]]
trigger = "ctrl+alt+1"
action = "brightness_up"
value = "10"

[[ binding ]]
trigger = "ctrl+alt+2"
action = "brightness_down"
value = "10"

[[ binding ]]
trigger = "ctrl+alt+q"
action = "quit"
        "#
    }

    fn parse(s: &str) -> AppConfig {
        AppConfig::parse(s, Path::new("test.toml")).unwrap()
    }

    // ── Valid TOML ───────────────────────────────────────────────

    #[test]
    fn parse_valid_config() {
        let cfg = parse(valid_config());
        assert_eq!(cfg.active_bindings().len(), 3);
        assert_eq!(cfg.volume_step(), 1);
        assert!(cfg.theme.is_none());
    }

    #[test]
    fn parse_config_with_theme() {
        let toml = format!(
            r#"theme = "carbon"
{}
        "#,
            valid_config()
        );
        let cfg = parse(&toml);
        assert_eq!(cfg.theme.as_deref(), Some("carbon"));
    }

    #[test]
    fn parse_config_with_volume_step() {
        let toml = format!(
            r#"volume_step = 5
{}
        "#,
            valid_config()
        );
        let cfg = parse(&toml);
        assert_eq!(cfg.volume_step(), 5);
    }

    #[test]
    fn parse_config_with_autostart() {
        let toml = format!(
            r#"autostart = true
{}
        "#,
            valid_config()
        );
        let cfg = parse(&toml);
        assert!(cfg.autostart());
    }

    #[test]
    fn parse_invalid_action_type() {
        // An unknown action type causes the binding to be skipped
        let toml = r#"
[[ binding ]]
trigger = "ctrl+alt+1"
action = "nonexistent_action_type"
        "#;
        let cfg = AppConfig::parse(toml, Path::new("test.toml")).unwrap();
        assert_eq!(cfg.active_bindings().len(), 0);
    }

    #[test]
    fn parse_invalid_trigger_skips_binding() {
        // An invalid trigger causes the binding to be skipped
        let toml = r#"
[[ binding ]]
trigger = ""
action = "quit"
        "#;
        let cfg = AppConfig::parse(toml, Path::new("test.toml")).unwrap();
        assert_eq!(cfg.active_bindings().len(), 0);
    }

    #[test]
    fn parse_missing_required_field() {
        // replace_key requires 'keys' field
        let toml = r#"
[[ binding ]]
trigger = "ctrl+alt+1"
action = "replace_key"
        "#;
        let result = AppConfig::parse(toml, Path::new("test.toml"));
        assert!(result.is_ok());
        let cfg = result.unwrap();
        assert_eq!(cfg.active_bindings().len(), 0);
    }

    #[test]
    fn parse_with_scheme_switch() {
        let toml = r#"
active_scheme = "gaming"

[[ binding ]]
trigger = "ctrl+alt+1"
action = "quit"
scheme = "gaming"

[[ binding ]]
trigger = "ctrl+alt+1"
action = "brightness_up"
scheme = "default"
        "#;
        let mut cfg = parse(toml);
        // Active scheme is "gaming", so only the gaming binding is active
        assert_eq!(cfg.active_bindings().len(), 1);
        assert_eq!(cfg.active_bindings()[0].action.name(), "quit");

        // Switch to default scheme
        assert!(cfg.switch_scheme("default"));
        assert_eq!(cfg.active_bindings().len(), 1);
        assert_eq!(cfg.active_bindings()[0].action.name(), "brightness_up");
    }

    #[test]
    fn parse_switch_to_missing_scheme() {
        let mut cfg = parse(valid_config());
        assert!(!cfg.switch_scheme("nonexistent"));
    }

    #[test]
    fn parse_invalid_toml() {
        let result = AppConfig::parse("this is not toml [[[", Path::new("test.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn parse_empty_config() {
        let result = AppConfig::parse("", Path::new("test.toml"));
        assert!(result.is_ok());
        let cfg = result.unwrap();
        assert_eq!(cfg.active_bindings().len(), 0);
        assert_eq!(cfg.active_scheme(), "default");
    }

    #[test]
    fn parse_duplicate_trigger_accepted() {
        // Duplicate triggers produce a warning but parsing succeeds
        let toml = r#"
[[ binding ]]
trigger = "ctrl+alt+q"
action = "quit"

[[ binding ]]
trigger = "ctrl+alt+q"
action = "brightness_up"
        "#;
        let cfg = parse(toml);
        // Both bindings exist (2 total)
        assert_eq!(cfg.active_bindings().len(), 2);
        // build_trigger_map keeps the *first* occurrence for the active scheme
        let trigger = crate::trigger::parse_trigger("ctrl+alt+q").unwrap().trigger;
        assert_eq!(cfg.lookup_trigger(&trigger).unwrap().action.name(), "quit");
    }

    // ── Integration test ──────────────────────────────────────────

    #[test]
    fn integration_load_sample_config() {
        let toml = r#"
theme = "built-in dark"
volume_step = 2
active_scheme = "default"

[[ binding ]]
trigger = "ctrl+win+s"
action = "replace_key"
keys = "ctrl+shift+s"

[[ binding ]]
trigger = "alt+f4"
action = "run_ps"
command = "echo hello"

[[ binding ]]
trigger = "ctrl+alt+b"
action = "brightness_up"
value = "15"

[[ binding ]]
trigger = "ctrl+alt+v"
action = "show_volume_mixer"

[[ binding ]]
trigger = "ctrl+alt+m"
action = "media_mute"

[[ binding ]]
trigger = "ctrl+alt+t"
action = "toggle_topmost"

[[ binding ]]
trigger = "ctrl+alt+p"
action = "pomodoro"

[[ binding ]]
trigger = "ctrl+alt+q"
action = "quit"
        "#;

        let cfg = AppConfig::parse(toml, Path::new("test.toml")).unwrap();

        // Verify number of active bindings
        assert_eq!(cfg.active_bindings().len(), 8);

        // Verify volume_step
        assert_eq!(cfg.volume_step(), 2);

        // Verify theme
        assert_eq!(cfg.theme.as_deref(), Some("built-in dark"));

        // Verify action map lookups
        let check = |trigger_str: &str, expected_action: &str| {
            let t = crate::trigger::parse_trigger(trigger_str).unwrap().trigger;
            let binding = cfg.lookup_trigger(&t);
            assert!(binding.is_some(), "trigger '{}' not found", trigger_str);
            assert_eq!(binding.unwrap().action.name(), expected_action);
        };

        check("ctrl+win+s", "replace_key");
        check("alt+f4", "run_ps");
        check("ctrl+alt+b", "brightness_up");
        check("ctrl+alt+v", "show_volume_mixer");
        check("ctrl+alt+m", "media_mute");
        check("ctrl+alt+t", "toggle_topmost");
        check("ctrl+alt+p", "pomodoro");
        check("ctrl+alt+q", "quit");

        // Test that unknown triggers return None
        let unknown = crate::trigger::parse_trigger("ctrl+alt+z").unwrap().trigger;
        assert!(cfg.lookup_trigger(&unknown).is_none());
    }
}
