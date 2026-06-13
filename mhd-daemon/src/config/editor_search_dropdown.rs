#[derive(Clone)]
pub struct SearchDropdownItem {
    pub id: usize,
    pub label: String,
    pub description: Option<String>,
    pub keywords: Vec<String>,
}

impl SearchDropdownItem {
    pub fn new(id: usize, label: impl Into<String>, keywords: Vec<String>) -> Self {
        Self {
            id,
            label: label.into(),
            description: None,
            keywords,
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

#[derive(Default)]
pub struct SearchDropdownState {
    pub is_open: bool,
    pub filter: String,
    pub scroll: usize,
}

impl SearchDropdownState {
    pub fn open(&mut self, items: &[SearchDropdownItem], selected_id: usize, visible_rows: usize) {
        self.is_open = true;
        self.filter.clear();
        self.scroll = 0;
        self.scroll_selected_into_view(items, selected_id, visible_rows);
    }

    pub fn close(&mut self) {
        self.is_open = false;
        self.filter.clear();
        self.scroll = 0;
    }

    pub fn input_char(&mut self, ch: char, items: &[SearchDropdownItem], visible_rows: usize) {
        if ch.is_ascii_graphic() || ch == ' ' {
            self.filter.push(ch);
            self.scroll = 0;
            self.clamp_scroll(items, visible_rows);
        }
    }

    pub fn backspace(&mut self, items: &[SearchDropdownItem], visible_rows: usize) {
        self.filter.pop();
        self.scroll = 0;
        self.clamp_scroll(items, visible_rows);
    }

    pub fn scroll_by(&mut self, delta_rows: i32, items: &[SearchDropdownItem], visible_rows: usize) {
        let max_scroll = self.filtered_count(items).saturating_sub(visible_rows);
        if delta_rows < 0 {
            self.scroll = self.scroll.saturating_sub(delta_rows.unsigned_abs() as usize);
        } else {
            self.scroll = (self.scroll + delta_rows as usize).min(max_scroll);
        }
    }

    pub fn visible_items<'a>(
        &self,
        items: &'a [SearchDropdownItem],
        visible_rows: usize,
    ) -> Vec<&'a SearchDropdownItem> {
        self.filtered_items(items)
            .into_iter()
            .skip(self.scroll)
            .take(visible_rows)
            .collect()
    }

    pub fn filtered_count(&self, items: &[SearchDropdownItem]) -> usize {
        self.filtered_items(items).len()
    }

    fn scroll_selected_into_view(
        &mut self,
        items: &[SearchDropdownItem],
        selected_id: usize,
        visible_rows: usize,
    ) {
        let selected = self
            .filtered_items(items)
            .iter()
            .position(|item| item.id == selected_id)
            .unwrap_or(0);
        if selected >= visible_rows {
            self.scroll = selected.saturating_sub(visible_rows - 1);
        }
        self.clamp_scroll(items, visible_rows);
    }

    fn clamp_scroll(&mut self, items: &[SearchDropdownItem], visible_rows: usize) {
        let max_scroll = self.filtered_count(items).saturating_sub(visible_rows);
        self.scroll = self.scroll.min(max_scroll);
    }

    fn filtered_items<'a>(&self, items: &'a [SearchDropdownItem]) -> Vec<&'a SearchDropdownItem> {
        let query = self.filter.trim().to_lowercase();
        items
            .iter()
            .filter(|item| {
                query.is_empty()
                    || item.label.to_lowercase().contains(&query)
                    || item
                        .description
                        .as_ref()
                        .is_some_and(|description| description.to_lowercase().contains(&query))
                    || item
                        .keywords
                        .iter()
                        .any(|keyword| keyword.to_lowercase().contains(&query))
            })
            .collect()
    }
}
