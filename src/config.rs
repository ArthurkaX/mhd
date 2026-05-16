use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Deserialize;

use crate::action::Action;
use crate::trigger::parse_trigger;

/// Raw TOML binding entry.
#[derive(Debug, Deserialize)]
struct RawBinding {
    trigger: String,
    action: String,
    #[serde(default)]
    scheme: Option<String>,
    #[serde(default)]
    keys: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    target_scheme: Option<String>,
}

/// Top-level TOML config structure.
#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    active_scheme: Option<String>,
    #[serde(default)]
    binding: Vec<RawBinding>,
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
}

impl AppConfig {
    /// Parse and validate config from a TOML string.
    pub fn parse(content: &str, _path: &Path) -> Result<Self, String> {
        let raw: RawConfig = toml::from_str(content).map_err(|e| {
            format!("config parse error: {e}")
        })?;

        let active_scheme = raw.active_scheme.unwrap_or_else(|| "default".to_string());
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
            let scheme = raw_b.scheme.clone().unwrap_or_else(|| "default".to_string());

            // Parse trigger
            let parsed_trigger = parse_trigger(&raw_b.trigger)?;

            // Validate and create action
            let action = match raw_b.action.as_str() {
                "replace_key" => {
                    let keys = raw_b.keys.as_ref()
                        .ok_or_else(|| "replace_key action requires 'keys' field".to_string())?;
                    Action::new_replace_key(keys)?
                }
                "run_ps" => {
                    let command = raw_b.command.as_ref()
                        .ok_or_else(|| "run_ps action requires 'command' field".to_string())?;
                    Action::new_run_ps(command)?
                }
                "switch_scheme" => {
                    let target = raw_b.target_scheme.as_ref()
                        .ok_or_else(|| "switch_scheme action requires 'target_scheme' field".to_string())?;
                    Action::new_switch_scheme(target)?
                }
                other => {
                    return Err(format!("unknown action: {other}"));
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
            if let Action::SwitchScheme { target_scheme } = &binding.action {
                if !scheme_names.contains(target_scheme) {
                    return Err(format!(
                        "switch_scheme target '{}' does not exist in config",
                        target_scheme
                    ));
                }
            }
        }

        // Check for duplicate triggers within same scheme
        let mut seen_triggers: HashMap<(String, crate::trigger::Trigger), ()> = HashMap::new();
        for binding in &bindings {
            let key = (binding.scheme.clone(), binding.trigger);
            if seen_triggers.contains_key(&key) {
                return Err(format!(
                    "duplicate trigger '{}' in scheme '{}'",
                    binding.trigger_name, binding.scheme
                ));
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
        })
    }

    fn build_trigger_map(bindings: &[Binding], active_scheme: &str) -> HashMap<crate::trigger::Trigger, usize> {
        let mut map = HashMap::new();
        for (i, binding) in bindings.iter().enumerate() {
            if binding.scheme == active_scheme {
                map.insert(binding.trigger, i);
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
        self.trigger_map
            .get(trigger)
            .map(|&i| &self.bindings[i])
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
}
