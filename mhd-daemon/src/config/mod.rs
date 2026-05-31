pub mod path;
pub mod raw;
pub mod editor;
pub mod editor_layout;
pub mod editor_state;
pub mod editor_hittest;
pub mod editor_theme;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::action::Action;
use crate::trigger::parse_trigger;
#[cfg(feature = "blackbox")]
use crate::blackbox::BlackboxConfig;
use crate::overlays::note::QuickNoteConfig;
use crate::config::path::home_dir;
use self::raw::RawConfig;

fn default_notes_dir() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("mhd")
        .join("notes")
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
    /// Ordered list of power plan names for rotation.
    pub power_plans: Vec<String>,
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
                && !scheme_names.contains(target_scheme) {
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
                enabled: raw.blackbox.as_ref().and_then(|b| b.enabled).unwrap_or(false),
                idle_seconds: raw.blackbox.as_ref().and_then(|b| b.idle_seconds).unwrap_or(300),
            },
            quicknote: QuickNoteConfig {
                enabled: raw.quicknote.as_ref().and_then(|q| q.enabled).unwrap_or(true),
                notes_dir: raw.quicknote.as_ref().and_then(|q| q.notes_dir.as_ref()).map(PathBuf::from).unwrap_or_else(default_notes_dir),
            },
            autostart: raw.autostart.unwrap_or(false),
            power_plans: raw.power_plans,
        })
    }

    fn build_trigger_map(
        bindings: &[Binding],
        active_scheme: &str,
    ) -> HashMap<crate::trigger::Trigger, usize> {
        let mut map = HashMap::new();
        for (i, binding) in bindings.iter().enumerate() {
            if binding.scheme == active_scheme {
                if map.contains_key(&binding.trigger) {
                    eprintln!(
                        "mhd: warning — duplicate trigger '{}' in scheme '{}', ignoring",
                        binding.trigger_name, active_scheme
                    );
                } else {
                    map.insert(binding.trigger, i);
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
