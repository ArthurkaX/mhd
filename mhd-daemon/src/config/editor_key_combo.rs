use crate::config::editor_search_dropdown::{SearchDropdownItem, SearchDropdownState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyComboSlot {
    Modifier(usize),
    Key,
}

pub struct KeyComboEditorState {
    modifiers: [Option<&'static str>; 3],
    key: Option<String>,
    pub dropdown: SearchDropdownState,
    pub open_slot: Option<KeyComboSlot>,
}

impl KeyComboEditorState {
    pub fn from_trigger_string(value: &str) -> Self {
        let mut state = Self {
            modifiers: [None, None, None],
            key: None,
            dropdown: SearchDropdownState::default(),
            open_slot: None,
        };
        state.set_from_string(value);
        state
    }

    pub fn set_from_string(&mut self, value: &str) {
        self.modifiers = [None, None, None];
        self.key = None;

        let mut mod_i = 0;
        for part in value.split('+').map(|p| p.trim().to_lowercase()) {
            match part.as_str() {
                "ctrl" | "control" => {
                    if mod_i < self.modifiers.len() {
                        self.modifiers[mod_i] = Some("ctrl");
                        mod_i += 1;
                    }
                }
                "alt" => {
                    if mod_i < self.modifiers.len() {
                        self.modifiers[mod_i] = Some("alt");
                        mod_i += 1;
                    }
                }
                "shift" => {
                    if mod_i < self.modifiers.len() {
                        self.modifiers[mod_i] = Some("shift");
                        mod_i += 1;
                    }
                }
                "win" | "super" => {
                    if mod_i < self.modifiers.len() {
                        self.modifiers[mod_i] = Some("win");
                        mod_i += 1;
                    }
                }
                "" | "none" => {}
                _ => self.key = Some(part),
            }
        }
    }

    pub fn set_from_capture(&mut self, value: &str) {
        self.set_from_string(value);
        self.close_dropdown();
    }

    pub fn to_trigger_string(&self) -> String {
        let mut parts: Vec<String> = self
            .modifiers
            .iter()
            .filter_map(|m| m.map(str::to_string))
            .collect();
        if let Some(key) = &self.key {
            if !key.is_empty() {
                parts.push(key.clone());
            }
        }
        if parts.is_empty() {
            "none".to_string()
        } else {
            parts.join("+")
        }
    }

    pub fn slot_label(&self, slot: KeyComboSlot) -> String {
        match slot {
            KeyComboSlot::Modifier(i) => self.modifiers[i].unwrap_or("none").to_string(),
            KeyComboSlot::Key => self.key.clone().unwrap_or_else(|| "none".to_string()),
        }
    }

    pub fn open_dropdown(&mut self, slot: KeyComboSlot, visible_rows: usize) {
        let selected_id = self.selected_item_id(slot);
        let items = self.items_for_slot(slot);
        self.open_slot = Some(slot);
        self.dropdown.open(&items, selected_id, visible_rows);
    }

    pub fn close_dropdown(&mut self) {
        self.open_slot = None;
        self.dropdown.close();
    }

    pub fn choose(&mut self, id: usize) {
        if let Some(slot) = self.open_slot {
            match slot {
                KeyComboSlot::Modifier(i) => {
                    self.modifiers[i] = match id {
                        1 => Some("ctrl"),
                        2 => Some("alt"),
                        3 => Some("shift"),
                        4 => Some("win"),
                        _ => None,
                    };
                    self.dedupe_modifiers();
                }
                KeyComboSlot::Key => {
                    self.key = if id == 0 {
                        None
                    } else {
                        self.items_for_slot(slot).get(id).map(|item| item.label.clone())
                    };
                }
            }
        }
        self.close_dropdown();
    }

    pub fn items_for_open_slot(&self) -> Vec<SearchDropdownItem> {
        self.open_slot
            .map(|slot| self.items_for_slot(slot))
            .unwrap_or_default()
    }

    fn selected_item_id(&self, slot: KeyComboSlot) -> usize {
        match slot {
            KeyComboSlot::Modifier(i) => match self.modifiers[i] {
                Some("ctrl") => 1,
                Some("alt") => 2,
                Some("shift") => 3,
                Some("win") => 4,
                _ => 0,
            },
            KeyComboSlot::Key => self
                .key
                .as_ref()
                .and_then(|key| {
                    self.items_for_slot(slot)
                        .iter()
                        .position(|item| item.label == *key)
                })
                .unwrap_or(0),
        }
    }

    fn items_for_slot(&self, slot: KeyComboSlot) -> Vec<SearchDropdownItem> {
        match slot {
            KeyComboSlot::Modifier(_) => ["none", "ctrl", "alt", "shift", "win"]
                .iter()
                .enumerate()
                .map(|(id, label)| SearchDropdownItem::new(id, *label, vec![label.to_string()]))
                .collect(),
            KeyComboSlot::Key => std::iter::once(SearchDropdownItem::new(0, "none", vec![]))
                .chain(
                    crate::core::trigger::known_key_names()
                        .into_iter()
                        .enumerate()
                        .map(|(idx, name)| SearchDropdownItem::new(idx + 1, name.clone(), vec![name])),
                )
                .collect(),
        }
    }

    fn dedupe_modifiers(&mut self) {
        let mut seen = Vec::new();
        for modifier in &mut self.modifiers {
            if let Some(value) = *modifier {
                if seen.contains(&value) {
                    *modifier = None;
                } else {
                    seen.push(value);
                }
            }
        }
    }
}
