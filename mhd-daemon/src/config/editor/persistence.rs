//! Configuration loading, applying, and saving for the Settings panel.

use super::*;
use crate::app::AppHandle;
use crate::app::DaemonControl;
use crate::core::native_theme::load_theme_from_path;
use crate::core::trigger::{KeyCombo, keys_to_string, parse_trigger};
use crate::overlays::keycast::KeycastPosition;

/// Load the currently active UI bindings from the daemon config.
pub(crate) fn load_ui_bindings(handle: &AppHandle) -> Vec<UIBinding> {
    use crate::action::Action;

    let config = handle.config.lock().unwrap();
    config
        .active_bindings()
        .iter()
        .map(|b| {
            let kind_idx = editor_index_for_action_name(b.action.name());

            let param = match &b.action {
                Action::ReplaceKey { keys } => keys_to_string(keys),
                Action::RunPs { command } => command.clone(),
                Action::SetBrightness { relative, value } => {
                    if *relative {
                        format!("{:+}", value)
                    } else {
                        format!("{}", value)
                    }
                }
                Action::BrightnessUp { value } | Action::BrightnessDown { value } => {
                    value.to_string()
                }
                _ => String::new(),
            };

            UIBinding {
                trigger: b.trigger_name.clone(),
                kind_idx,
                param,
                is_recording_trigger: false,
                is_recording_param: false,
            }
        })
        .collect()
}

// ═══════════════════════════════════════════════════════════════════════
// Apply & Save
// ═══════════════════════════════════════════════════════════════════════

pub(crate) fn apply_settings(state: &mut SettingsState) {
    let theme_name = state
        .theme_names
        .get(state.theme_sel)
        .cloned()
        .unwrap_or_else(|| "Code".to_string());

    let config_name = if theme_name == "Code" {
        String::new()
    } else {
        let themes_dir = crate::native_theme::themes_dir();
        let mut found = String::new();
        if let Ok(entries) = std::fs::read_dir(&themes_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    if let Ok(t) = load_theme_from_path(&path)
                        && t.name == theme_name
                    {
                        found = stem.to_string();
                        break;
                    }
                    if stem == theme_name {
                        found = stem.to_string();
                        break;
                    }
                }
            }
        }
        if found.is_empty() {
            theme_name.clone()
        } else {
            found
        }
    };

    if let Err(e) = save_config(
        &state.handle.config_path,
        &config_name,
        &state.bindings,
        state.autostart,
        &state.notes_dir,
        &state.draw_dir,
        state.keycast_position,
        state.keycast_duration_ms,
        state.keycast_show_typing,
        state.keycast_typing_width_chars,
        state.keycast_typing_duration_ms,
        &state.handle,
    ) {
        eprintln!("mhd: settings error: {e}");
        return;
    }

    // Persist providers and models to the proxy config
    {
        let mut providers: Vec<llm_proxy::config::Provider> = state
            .providers
            .iter()
            .map(|p| llm_proxy::config::Provider {
                name: p.name.clone(),
                endpoint: p.endpoint.clone(),
                api: None,
            })
            .collect();

        // The settings UI has no `api` control; carry overrides from disk.
        llm_proxy::config::preserve_provider_apis(&mut providers);

        if let Err(e) = llm_proxy::config::save_providers(&providers) {
            eprintln!("mhd: failed to save providers: {e}");
        }

        // Models: each UiProvider model becomes a Model tied to that provider.
        let mut models: Vec<llm_proxy::config::Model> = state
            .providers
            .iter()
            .flat_map(|p| {
                p.models.iter().map(|m| llm_proxy::config::Model {
                    provider: p.name.clone(),
                    id: m.clone(),
                    display_name: m.clone(),
                    tags: vec![],
                    api: None,
                })
            })
            .collect();

        // Carry per-model `api` overrides from disk (UI has no `api` control).
        llm_proxy::config::preserve_model_apis(&mut models);

        if let Err(e) = llm_proxy::config::save_models(&models) {
            eprintln!("mhd: failed to save models: {e}");
        }

        // Save per-provider API keys + upstream_key fallback + anthropic_key.
        let mut provider_keys = std::collections::HashMap::new();
        for p in &state.providers {
            if !p.api_key.is_empty() {
                provider_keys.insert(p.name.clone(), p.api_key.clone());
            }
        }
        let api_key = state.providers.first().and_then(|p| {
            if p.api_key.is_empty() {
                None
            } else {
                Some(p.api_key.clone())
            }
        });
        if api_key.is_some() || !state.anthropic_key.is_empty() || !provider_keys.is_empty() {
            let secrets = llm_proxy::config::Secrets {
                anthropic_key: state.anthropic_key.clone(),
                upstream_key: api_key.unwrap_or_default(),
                provider_keys,
            };
            if let Err(e) = llm_proxy::config::save_secrets(&secrets) {
                eprintln!("mhd: failed to save secrets: {e}");
            }
        }

        // Save proxy bind address (port + IP) to settings.json
        if let Ok(mut settings) = llm_proxy::config::load_settings() {
            // Parse bind address like "127.0.0.1:8317"
            if let Some((ip, port_str)) = state.proxy_bind_address.rsplit_once(':') {
                if let Ok(port) = port_str.parse::<u16>() {
                    settings.port = port;
                }
                if !ip.is_empty() {
                    settings.bind_ip = ip.to_string();
                }
            }
            settings.opus_downgrade_enabled = state.opus_downgrade_enabled;
            settings.sonnet_downgrade_enabled = state.sonnet_downgrade_enabled;
            settings.trim_enabled = state.trim_enabled;
            settings.trim_openai_enabled = Some(state.trim_openai_enabled);
            settings.trim_codex_enabled = state.trim_codex_enabled;
            settings.trim_tool_desc_chars = state.trim_tool_desc_chars;
            settings.trim_toolresult_head = state.trim_toolresult_head;
            settings.trim_head_haiku = state.trim_head_haiku;
            settings.trim_head_harness = state.trim_head_harness;
            settings.trim_toolresult_tail = state.trim_toolresult_tail;
            settings.trim_ws_enabled = state.trim_ws_enabled;
            settings.trim_strip_thinking = state.trim_strip_thinking;
            settings.trim_free_target = state.trim_free_target.clone();
            settings.vision_model = state.vision_model.clone();
            settings.vision_prompt = state.vision_prompt.clone();
            if let Err(e) = llm_proxy::config::save_settings(&settings) {
                eprintln!("mhd: failed to save proxy settings: {e}");
            }
        }
    }

    if let Err(e) = state.handle.reload_config() {
        eprintln!("mhd: settings reload error: {e}");
        return;
    }

    state.theme = state.handle.theme();
    crate::keycast::sync_config(state.theme.clone(), state.handle.keycast_config());
}

pub(crate) fn save_config(
    path: &std::path::Path,
    theme: &str,
    bindings: &[UIBinding],
    autostart: bool,
    notes_dir: &std::path::Path,
    draw_dir: &std::path::Path,
    keycast_position: KeycastPosition,
    keycast_duration_ms: u64,
    keycast_show_typing: bool,
    keycast_typing_width_chars: u32,
    keycast_typing_duration_ms: u64,
    handle: &AppHandle,
) -> Result<(), String> {
    {
        let mut seen = std::collections::HashSet::new();
        for b in bindings {
            // Canonicalize so alias spellings of the same physical key
            // (e.g. "0x13" vs "pause") collapse to one entry.
            let key = match parse_trigger(&b.trigger) {
                Ok(pt) => keys_to_string(&KeyCombo {
                    modifiers: pt.trigger.modifiers,
                    key: Some(pt.trigger.key),
                }),
                Err(_) => b.trigger.trim().to_lowercase(),
            };
            if !seen.insert(key) {
                return Err(format!(
                    "Duplicate trigger '{}' — each trigger must be unique within the active scheme",
                    b.trigger
                ));
            }
        }
    }

    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut toml_val: toml::Value = if content.trim().is_empty() {
        toml::Value::Table(toml::value::Table::new())
    } else {
        toml::from_str(&content).map_err(|e| e.to_string())?
    };

    let active_scheme = handle.config.lock().unwrap().active_scheme().to_string();

    if let Some(table) = toml_val.as_table_mut() {
        if theme.is_empty() {
            table.remove("theme");
        } else {
            table.insert("theme".to_string(), toml::Value::String(theme.to_string()));
        }
        table.insert(
            "active_scheme".to_string(),
            toml::Value::String(active_scheme),
        );

        if autostart {
            table.insert("autostart".to_string(), toml::Value::Boolean(true));
        } else {
            table.remove("autostart");
        }

        {
            let qn = table
                .entry("quicknote".to_string())
                .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
            if let Some(qn_table) = qn.as_table_mut() {
                qn_table.insert(
                    "notes_dir".to_string(),
                    toml::Value::String(notes_dir.to_string_lossy().into_owned()),
                );
            }
        }

        {
            let qd = table
                .entry("quickdraw".to_string())
                .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
            if let Some(qd_table) = qd.as_table_mut() {
                qd_table.insert(
                    "draw_dir".to_string(),
                    toml::Value::String(draw_dir.to_string_lossy().into_owned()),
                );
            }
        }

        {
            let kc = table
                .entry("keycast".to_string())
                .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
            if let Some(kc_table) = kc.as_table_mut() {
                kc_table.insert(
                    "position".to_string(),
                    toml::Value::String(keycast_position.config_value().to_string()),
                );
                kc_table.insert(
                    "duration_ms".to_string(),
                    toml::Value::Integer(keycast_duration_ms as i64),
                );
                kc_table.insert(
                    "show_typing".to_string(),
                    toml::Value::Boolean(keycast_show_typing),
                );
                kc_table.insert(
                    "typing_width_chars".to_string(),
                    toml::Value::Integer(keycast_typing_width_chars as i64),
                );
                kc_table.insert(
                    "typing_duration_ms".to_string(),
                    toml::Value::Integer(keycast_typing_duration_ms as i64),
                );
            }
        }

        let mut new_bindings = Vec::new();
        for b in bindings {
            let mut map = toml::value::Table::new();
            map.insert(
                "trigger".to_string(),
                toml::Value::String(b.trigger.clone()),
            );
            let desc = editor_action_desc(b.kind_idx);
            map.insert(
                "action".to_string(),
                toml::Value::String(desc.name.to_string()),
            );
            if let Some(param_key) = desc.param_key {
                map.insert(param_key.to_string(), toml::Value::String(b.param.clone()));
            }
            new_bindings.push(toml::Value::Table(map));
        }
        table.insert("binding".to_string(), toml::Value::Array(new_bindings));
    }

    let new_content = toml::to_string_pretty(&toml_val).map_err(|e| e.to_string())?;
    std::fs::write(path, new_content).map_err(|e| e.to_string())?;
    Ok(())
}
