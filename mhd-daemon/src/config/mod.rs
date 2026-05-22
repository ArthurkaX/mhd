pub mod path;
pub mod raw;
pub mod editor;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::action::Action;
use crate::trigger::parse_trigger;
#[cfg(feature = "blackbox")]
use crate::blackbox::BlackboxConfig;
use self::raw::RawConfig;

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
    pub autostart: bool,
    /// Behavioural logger config.
    #[cfg(feature = "blackbox")]
    pub blackbox: BlackboxConfig,
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
            autostart: raw.autostart.unwrap_or(false),
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
    pub fn autostart(&self) -> bool {
        self.autostart
    }

    /// Blackbox configuration.
    #[cfg(feature = "blackbox")]
    pub fn blackbox(&self) -> &BlackboxConfig {
        &self.blackbox
    }
}
