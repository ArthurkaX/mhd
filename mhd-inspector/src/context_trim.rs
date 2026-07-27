//! Context Trim inspector — standalone or embedded in the monitor window.
//!
//! Preserves the original semantics: load a proxy.db request body, show the
//! before/after trim tree, knobs, composition grid, and diff panel.

use std::path::PathBuf;

use eframe::egui;
use serde_json::Value;

use llm_proxy::db_log::decompress_body;
use llm_proxy::native_trim::{trim_native, NativeKnobs};

// ── Token estimator ──────────────────────────────────────────────────────────

/// ~4 chars/token over serialized JSON. NOT a real tokenizer; matches the
/// llm-proxy bench/backtest estimator so numbers line up.
pub fn est_tokens(v: &Value) -> u64 {
    (serde_json::to_string(v).map(|s| s.len()).unwrap_or(0) as u64) / 4
}

// ── NodePath ─────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Hash)]
pub enum PathSegment {
    Key(String),
    Index(usize),
}

pub type NodePath = Vec<PathSegment>;

pub fn extract<'a>(root: &'a Value, path: &NodePath) -> Option<&'a Value> {
    let mut current = root;
    for seg in path {
        match seg {
            PathSegment::Key(k) => current = current.get(k.as_str())?,
            PathSegment::Index(i) => current = current.get(*i)?,
        }
    }
    Some(current)
}

// ── Tree model ───────────────────────────────────────────────────────────────

pub struct Node {
    pub label: String,
    pub path: NodePath,
    /// Byte length of the serialized node before/after trim. Kept for a future
    /// byte-size column; the tree currently displays token estimates only.
    #[allow(dead_code)]
    pub size_before: usize,
    pub tok_before: u64,
    #[allow(dead_code)]
    pub size_after: usize,
    pub tok_after: u64,
    pub children: Vec<Node>,
}

pub fn build_tree(before: &Value, after: &Value) -> Vec<Node> {
    let mut nodes = Vec::new();

    // ── system ──
    if let Some(sys) = before.get("system") {
        let sys_after = after.get("system");
        nodes.push(Node {
            label: "system".into(),
            path: vec![PathSegment::Key("system".into())],
            size_before: serde_json::to_string(sys).unwrap_or_default().len(),
            tok_before: est_tokens(sys),
            size_after: sys_after
                .map(|v| serde_json::to_string(v).unwrap_or_default().len())
                .unwrap_or(0),
            tok_after: sys_after.map(est_tokens).unwrap_or(0),
            children: vec![],
        });
    }

    // ── tools ──
    if let Some(tools) = before.get("tools").and_then(|v| v.as_array()) {
        let tools_after = after.get("tools").and_then(|v| v.as_array());

        let total_size: usize = tools
            .iter()
            .map(|t| serde_json::to_string(t).unwrap_or_default().len())
            .sum();
        let total_tok: u64 = tools.iter().map(est_tokens).sum();
        let after_total_size: usize = tools_after
            .map(|a| {
                a.iter()
                    .map(|t| serde_json::to_string(t).unwrap_or_default().len())
                    .sum()
            })
            .unwrap_or(0);
        let after_total_tok: u64 = tools_after
            .map(|a| a.iter().map(est_tokens).sum())
            .unwrap_or(0);

        let mut children = Vec::with_capacity(tools.len());
        for (i, tool) in tools.iter().enumerate() {
            let name = tool
                .get("name")
                .and_then(|n| n.as_str())
                .or_else(|| tool.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()))
                .unwrap_or("?")
                .to_string();
            let after_tool = tools_after.and_then(|a| a.get(i));
            children.push(Node {
                label: format!("[{}] {}", i, name),
                path: vec![PathSegment::Key("tools".into()), PathSegment::Index(i)],
                size_before: serde_json::to_string(tool).unwrap_or_default().len(),
                tok_before: est_tokens(tool),
                size_after: after_tool
                    .map(|v| serde_json::to_string(v).unwrap_or_default().len())
                    .unwrap_or(0),
                tok_after: after_tool.map(est_tokens).unwrap_or(0),
                children: vec![],
            });
        }

        nodes.push(Node {
            label: format!("tools[{}]", tools.len()),
            path: vec![PathSegment::Key("tools".into())],
            size_before: total_size,
            tok_before: total_tok,
            size_after: after_total_size,
            tok_after: after_total_tok,
            children,
        });
    }

    // ── messages ──
    if let Some(msgs) = before.get("messages").and_then(|v| v.as_array()) {
        let msgs_after = after.get("messages").and_then(|v| v.as_array());

        let total_size: usize = msgs
            .iter()
            .map(|m| serde_json::to_string(m).unwrap_or_default().len())
            .sum();
        let total_tok: u64 = msgs.iter().map(est_tokens).sum();
        let after_total_size: usize = msgs_after
            .map(|a| {
                a.iter()
                    .map(|m| serde_json::to_string(m).unwrap_or_default().len())
                    .sum()
            })
            .unwrap_or(0);
        let after_total_tok: u64 = msgs_after
            .map(|a| a.iter().map(est_tokens).sum())
            .unwrap_or(0);

        let mut children = Vec::with_capacity(msgs.len());
        for (i, msg) in msgs.iter().enumerate() {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("?");
            let after_msg = msgs_after.and_then(|a| a.get(i));
            let size = serde_json::to_string(msg).unwrap_or_default().len();
            let tok = est_tokens(msg);
            let after_size = after_msg
                .map(|v| serde_json::to_string(v).unwrap_or_default().len())
                .unwrap_or(0);
            let after_tok = after_msg.map(est_tokens).unwrap_or(0);

            let mut grandchildren = Vec::new();
            if let Some(content) = msg.get("content").and_then(|c| c.as_array()) {
                for (j, block) in content.iter().enumerate() {
                    if block.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                        let after_block = after_msg.and_then(|a| {
                            a.get("content")
                                .and_then(|c| c.as_array())
                                .and_then(|a| a.get(j))
                        });
                        let tr_name = block
                            .get("tool_use_id")
                            .and_then(|id| id.as_str())
                            .unwrap_or("tool_result");
                        grandchildren.push(Node {
                            label: format!("  tool_result[{}] ({})", j, tr_name),
                            path: vec![
                                PathSegment::Key("messages".into()),
                                PathSegment::Index(i),
                                PathSegment::Key("content".into()),
                                PathSegment::Index(j),
                            ],
                            size_before: serde_json::to_string(block).unwrap_or_default().len(),
                            tok_before: est_tokens(block),
                            size_after: after_block
                                .map(|v| serde_json::to_string(v).unwrap_or_default().len())
                                .unwrap_or(0),
                            tok_after: after_block.map(est_tokens).unwrap_or(0),
                            children: vec![],
                        });
                    }
                }
            }

            children.push(Node {
                label: format!("[{}] {}", i, role),
                path: vec![PathSegment::Key("messages".into()), PathSegment::Index(i)],
                size_before: size,
                tok_before: tok,
                size_after: after_size,
                tok_after: after_tok,
                children: grandchildren,
            });
        }

        nodes.push(Node {
            label: format!("messages[{}]", msgs.len()),
            path: vec![PathSegment::Key("messages".into())],
            size_before: total_size,
            tok_before: total_tok,
            size_after: after_total_size,
            tok_after: after_total_tok,
            children,
        });
    }

    nodes
}

// ── Settings loading ─────────────────────────────────────────────────────────

pub fn load_settings_from_disk(knobs: &mut NativeKnobs) {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return,
    };
    let path = home.join(".config").join("mhd").join("llm-proxy").join("settings.json");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("settings.json: {e}");
            return;
        }
    };
    let json: Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("settings.json parse: {e}");
            return;
        }
    };
    let obj = match json.as_object() {
        Some(o) => o,
        None => return,
    };

    macro_rules! apply_u64 {
        ($key:expr => $field:ident) => {
            if let Some(v) = obj.get($key).and_then(|v| v.as_u64()) {
                knobs.$field = v as usize;
            }
        };
    }

    apply_u64!("trim_tool_desc_chars" => tool_max_desc_chars);
    apply_u64!("trim_toolresult_head" => tool_result_head);
    apply_u64!("trim_toolresult_tail" => tool_result_tail);

    if let Some(v) = obj.get("trim_ws_enabled").and_then(|v| v.as_bool()) {
        knobs.ws_enabled = v;
    }
    if let Some(v) = obj.get("trim_strip_thinking").and_then(|v| v.as_bool()) {
        knobs.strip_thinking = v;
    }
    if let Some(v) = obj.get("trim_fence_requires_code").and_then(|v| v.as_bool()) {
        knobs.tool_result_fence_requires_code = v;
    }
    if let Some(v) = obj.get("trim_arrow_density_min").and_then(|v| v.as_f64()) {
        knobs.tool_result_arrow_density_min = v;
    }
}

// ── Context grid ────────────────────────────────────────────────────────────

/// Category palette for the composition grid.
pub const COL_SYSTEM: egui::Color32 = egui::Color32::from_rgb(100, 150, 240);
pub const COL_TOOLS: egui::Color32 = egui::Color32::from_rgb(240, 180, 80);
pub const COL_MESSAGES: egui::Color32 = egui::Color32::from_rgb(80, 200, 140);
/// Empty cells in the "after" grid = tokens the trim freed.
pub const COL_SAVED: egui::Color32 = egui::Color32::from_rgb(215, 90, 90);
/// Empty cells in the "before" grid (should be none, but for rounding).
pub const COL_EMPTY: egui::Color32 = egui::Color32::from_rgb(224, 224, 224);

/// Top-level category token counts for a request body: (system, tools, messages).
pub fn category_tokens(body: &Value) -> (u64, u64, u64) {
    let sys = body.get("system").map(est_tokens).unwrap_or(0);
    let tools = body.get("tools").map(est_tokens).unwrap_or(0);
    let msgs = body.get("messages").map(est_tokens).unwrap_or(0);
    (sys, tools, msgs)
}

/// Paint a 10x10 composition grid.
pub fn draw_grid(
    ui: &mut egui::Ui,
    denom: u64,
    segs: &[(u64, egui::Color32)],
    free_color: egui::Color32,
) {
    const COLS: usize = 10;
    const ROWS: usize = 10;
    const CELLS: usize = COLS * ROWS;
    let cell = 9.0_f32;
    let gap = 2.0_f32;

    let size = egui::vec2(
        COLS as f32 * (cell + gap) - gap,
        ROWS as f32 * (cell + gap) - gap,
    );
    let (rect, _resp) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let per_cell = if denom == 0 { 1.0 } else { denom as f32 / CELLS as f32 };

    for k in 0..CELLS {
        let tok_at = (k as f32 + 0.5) * per_cell;
        let mut acc = 0.0_f32;
        let mut color = free_color;
        for (t, c) in segs {
            acc += *t as f32;
            if tok_at < acc {
                color = *c;
                break;
            }
        }
        let row = k / COLS;
        let col = k % COLS;
        let x = rect.min.x + col as f32 * (cell + gap);
        let y = rect.min.y + row as f32 * (cell + gap);
        let r = egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(cell, cell));
        painter.rect_filled(r, 2.0, color);
    }
}

// ── Tree UI rendering ────────────────────────────────────────────────────────

pub fn show_tree(ui: &mut egui::Ui, nodes: &[Node], selected: &mut Option<NodePath>) {
    for node in nodes {
        let delta_color = if node.tok_after < node.tok_before {
            egui::Color32::from_rgb(80, 200, 120)
        } else {
            egui::Color32::GRAY
        };

        let id = ui.make_persistent_id(&node.path);

        let header = egui::CollapsingHeader::new(&node.label)
            .id_salt(id)
            .default_open(false);

        if node.children.is_empty() {
            ui.horizontal(|ui| {
                let resp = ui.selectable_label(
                    selected.as_ref() == Some(&node.path),
                    &node.label,
                );
                if resp.clicked() {
                    *selected = Some(node.path.clone());
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = format!("{} -> {}", node.tok_before, node.tok_after);
                    ui.colored_label(delta_color, label);
                });
            });
        } else {
            header.show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.selectable_label(
                        selected.as_ref() == Some(&node.path),
                        &node.label,
                    )
                    .clicked()
                    {
                        *selected = Some(node.path.clone());
                    }

                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            let label = format!("{} -> {}", node.tok_before, node.tok_after);
                            ui.colored_label(delta_color, label);
                        },
                    );
                });

                show_tree(ui, &node.children, selected);
            });
        }
    }
}

// ── Row type and queries ─────────────────────────────────────────────────────

pub struct RequestRow {
    pub id: i64,
    pub run_id: i64,
    pub seq: i64,
    pub model: String,
    pub provider: String,
    pub body: Vec<u8>,
}

pub fn query_by_id(conn: &rusqlite::Connection, id: i64) -> Result<Option<RequestRow>, rusqlite::Error> {
    let sql = "SELECT id, run_id, seq, model, provider, body FROM request_bodies WHERE id = ?";
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query_map([id], |row| {
        Ok(RequestRow {
            id: row.get(0)?,
            run_id: row.get(1)?,
            seq: row.get(2)?,
            model: row.get(3)?,
            provider: row.get(4)?,
            body: row.get(5)?,
        })
    })?;
    match rows.next() {
        Some(Ok(r)) => Ok(Some(r)),
        Some(Err(e)) => Err(e),
        None => Ok(None),
    }
}

pub fn query_by_run_seq(
    conn: &rusqlite::Connection,
    run_id: i64,
    seq: i64,
) -> Result<Option<RequestRow>, rusqlite::Error> {
    let sql = "SELECT id, run_id, seq, model, provider, body FROM request_bodies WHERE run_id = ? AND seq = ? ORDER BY id DESC LIMIT 1";
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query_map(rusqlite::params![run_id, seq], |row| {
        Ok(RequestRow {
            id: row.get(0)?,
            run_id: row.get(1)?,
            seq: row.get(2)?,
            model: row.get(3)?,
            provider: row.get(4)?,
            body: row.get(5)?,
        })
    })?;
    match rows.next() {
        Some(Ok(r)) => Ok(Some(r)),
        Some(Err(e)) => Err(e),
        None => Ok(None),
    }
}

pub fn query_neighbor(
    conn: &rusqlite::Connection,
    cur: i64,
    dir: i64,
) -> Result<Option<RequestRow>, rusqlite::Error> {
    let sql = if dir < 0 {
        "SELECT id, run_id, seq, model, provider, body FROM request_bodies \
         WHERE id < ? ORDER BY id DESC LIMIT 1"
    } else {
        "SELECT id, run_id, seq, model, provider, body FROM request_bodies \
         WHERE id > ? ORDER BY id ASC LIMIT 1"
    };
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query_map([cur], |row| {
        Ok(RequestRow {
            id: row.get(0)?,
            run_id: row.get(1)?,
            seq: row.get(2)?,
            model: row.get(3)?,
            provider: row.get(4)?,
            body: row.get(5)?,
        })
    })?;
    match rows.next() {
        Some(Ok(r)) => Ok(Some(r)),
        Some(Err(e)) => Err(e),
        None => Ok(None),
    }
}

pub fn query_latest_anthropic(
    conn: &rusqlite::Connection,
) -> Result<Option<RequestRow>, rusqlite::Error> {
    let sql = "SELECT id, run_id, seq, model, provider, body FROM request_bodies WHERE provider = 'anthropic' ORDER BY id DESC LIMIT 1";
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query_map([], |row| {
        Ok(RequestRow {
            id: row.get(0)?,
            run_id: row.get(1)?,
            seq: row.get(2)?,
            model: row.get(3)?,
            provider: row.get(4)?,
            body: row.get(5)?,
        })
    })?;
    match rows.next() {
        Some(Ok(r)) => Ok(Some(r)),
        Some(Err(e)) => Err(e),
        None => Ok(None),
    }
}

// ── ContextTrimApp ───────────────────────────────────────────────────────────

/// Standalone Context Trim inspector — the original mhd-inspector behavior.
pub struct ContextTrimApp {
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub knobs: NativeKnobs,
    pub tree: Vec<Node>,
    pub selected_path: Option<NodePath>,
    pub status_message: String,
    pub run_id: i64,
    pub seq: i64,
    pub model: String,
    pub dirty: bool,
    pub db_path: PathBuf,
    pub row_id: Option<i64>,
}

impl ContextTrimApp {
    pub fn new(cc: &eframe::CreationContext<'_>, db_path: PathBuf, row_id: Option<i64>, run_id: Option<i64>, seq: Option<i64>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::light());

        let mut app = ContextTrimApp {
            before: None,
            after: None,
            knobs: NativeKnobs::default(),
            tree: Vec::new(),
            selected_path: None,
            status_message: format!("DB: {}", db_path.display()),
            run_id: 0,
            seq: 0,
            model: String::new(),
            dirty: false,
            db_path: db_path.clone(),
            row_id: None,
        };

        let conn = match rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) {
            Ok(c) => c,
            Err(e) => {
                app.status_message = format!("Cannot open DB: {e}");
                return app;
            }
        };

        let result = if let Some(id) = row_id {
            query_by_id(&conn, id)
        } else if let (Some(rid), Some(s)) = (run_id, seq) {
            query_by_run_seq(&conn, rid, s)
        } else {
            query_latest_anthropic(&conn)
        };

        match result {
            Ok(Some(row)) => app.apply_row(row),
            Ok(None) => app.status_message = "No body captured for that selection".into(),
            Err(e) => app.status_message = format!("Query error: {e}"),
        }

        app
    }

    pub fn apply_row(&mut self, row: RequestRow) {
        self.run_id = row.run_id;
        self.seq = row.seq;
        self.model = row.model.clone();
        self.row_id = Some(row.id);
        self.selected_path = None;
        self.status_message = format!(
            "id={} run_id={} seq={} model={} provider={}",
            row.id, row.run_id, row.seq, row.model, row.provider
        );

        match decompress_body(&row.body) {
            Some(body_str) => match serde_json::from_str::<Value>(&body_str) {
                Ok(val) => {
                    let after = trim_native(val.clone(), &self.knobs);
                    self.tree = build_tree(&val, &after);
                    self.before = Some(val);
                    self.after = Some(after);
                }
                Err(e) => {
                    self.before = None;
                    self.after = None;
                    self.tree.clear();
                    self.status_message = format!("JSON parse failed: {e}");
                }
            },
            None => {
                self.before = None;
                self.after = None;
                self.tree.clear();
                self.status_message = "Decompress failed for loaded body".into();
            }
        }
    }

    pub fn navigate(&mut self, dir: i64) {
        let Some(cur) = self.row_id else { return };
        let conn = match rusqlite::Connection::open_with_flags(
            &self.db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) {
            Ok(c) => c,
            Err(e) => {
                self.status_message = format!("Cannot open DB: {e}");
                return;
            }
        };
        match query_neighbor(&conn, cur, dir) {
            Ok(Some(row)) => self.apply_row(row),
            Ok(None) => {
                self.status_message = if dir < 0 {
                    "Already at the oldest captured request".into()
                } else {
                    "Already at the newest captured request".into()
                };
            }
            Err(e) => self.status_message = format!("Query error: {e}"),
        }
    }
}

impl eframe::App for ContextTrimApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let title = if self.model.is_empty() {
            "mhd-inspector".to_string()
        } else {
            format!(
                "mhd-inspector — run {} seq {} ({})",
                self.run_id, self.seq, self.model
            )
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));

        // ── Left panel: knobs ──
        egui::SidePanel::left("knobs")
            .resizable(true)
            .default_width(260.0)
            .show(ctx, |ui| {
                ui.heading("Native trim knobs");

                if let (Some(before), Some(after)) = (&self.before, &self.after) {
                    let bt = est_tokens(before);
                    let at = est_tokens(after);
                    if bt > 0 {
                        let pct = (bt.saturating_sub(at)) as f64 / bt as f64 * 100.0;
                        ui.label(format!("{} -> {}  (-{:.1}%)", bt, at, pct));
                    }
                }
                ui.separator();

                let r = ui.add(
                    egui::Slider::new(&mut self.knobs.tool_max_desc_chars, 0..=2000)
                        .text("tool_max_desc_chars"),
                );
                if r.changed() { self.dirty = true; }
                let r = ui.add(
                    egui::Slider::new(&mut self.knobs.tool_result_head, 0..=20000)
                        .text("tool_result_head"),
                );
                if r.changed() { self.dirty = true; }
                let r = ui.add(
                    egui::Slider::new(&mut self.knobs.tool_result_tail, 0..=10000)
                        .text("tool_result_tail"),
                );
                if r.changed() { self.dirty = true; }
                let r = ui.add(
                    egui::Slider::new(&mut self.knobs.tool_result_min_elide, 0..=20000)
                        .text("tool_result_min_elide"),
                );
                if r.changed() { self.dirty = true; }
                let r = ui.add(
                    egui::Slider::new(&mut self.knobs.ws_blank_run_max, 0..=20)
                        .text("ws_blank_run_max"),
                );
                if r.changed() { self.dirty = true; }

                let r = ui.checkbox(&mut self.knobs.ws_enabled, "ws_enabled");
                if r.changed() { self.dirty = true; }
                let r = ui.checkbox(&mut self.knobs.ws_strip_trailing, "ws_strip_trailing");
                if r.changed() { self.dirty = true; }
                let r = ui.checkbox(&mut self.knobs.ws_collapse_inner, "ws_collapse_inner");
                if r.changed() { self.dirty = true; }
                let r = ui.checkbox(&mut self.knobs.strip_thinking, "strip_thinking");
                if r.changed() { self.dirty = true; }
                let r = ui.checkbox(
                    &mut self.knobs.tool_result_fence_requires_code,
                    "tool_result_fence_requires_code",
                );
                if r.changed() { self.dirty = true; }

                let r = ui.add(
                    egui::Slider::new(&mut self.knobs.tool_result_arrow_density_min, 0.0..=0.2)
                        .text("tool_result_arrow_density_min"),
                );
                if r.changed() { self.dirty = true; }

                ui.separator();

                if ui.button("Reset to defaults").clicked() {
                    self.knobs = NativeKnobs::default();
                    self.dirty = true;
                }
                if ui.button("Load from settings.json").clicked() {
                    load_settings_from_disk(&mut self.knobs);
                    self.dirty = true;
                }

                if let (Some(before), Some(after)) = (&self.before, &self.after) {
                    ui.separator();
                    ui.strong("Distribution");

                    let (bs, bt, bm) = category_tokens(before);
                    let (as_, at, am) = category_tokens(after);
                    let denom = est_tokens(before).max(1);

                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(format!("Before {}", bs + bt + bm));
                            draw_grid(
                                ui,
                                denom,
                                &[(bs, COL_SYSTEM), (bt, COL_TOOLS), (bm, COL_MESSAGES)],
                                COL_EMPTY,
                            );
                        });
                        ui.add_space(10.0);
                        ui.vertical(|ui| {
                            ui.label(format!("After {}", as_ + at + am));
                            draw_grid(
                                ui,
                                denom,
                                &[(as_, COL_SYSTEM), (at, COL_TOOLS), (am, COL_MESSAGES)],
                                COL_SAVED,
                            );
                        });
                    });

                    let saved = est_tokens(before).saturating_sub(est_tokens(after));
                    let pct = saved as f64 / denom as f64 * 100.0;
                    ui.horizontal_wrapped(|ui| {
                        for (c, lbl) in [
                            (COL_SYSTEM, "system"),
                            (COL_TOOLS, "tools"),
                            (COL_MESSAGES, "messages"),
                            (COL_SAVED, "saved"),
                        ] {
                            let (r, _) =
                                ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                            ui.painter().rect_filled(r, 2.0, c);
                            ui.label(lbl);
                            ui.add_space(6.0);
                        }
                    });
                    ui.colored_label(COL_SAVED, format!("saved {saved} (-{pct:.1}%)"));
                }
            });

        if self.dirty {
            if let Some(before) = &self.before {
                let after = trim_native(before.clone(), &self.knobs);
                self.tree = build_tree(before, &after);
                self.after = Some(after);
            }
            self.dirty = false;
        }

        egui::TopBottomPanel::bottom("diff")
            .resizable(true)
            .default_height(200.0)
            .show(ctx, |ui| {
                if let Some(ref path) = self.selected_path {
                    if let (Some(before), Some(after)) = (&self.before, &self.after) {
                        ui.columns(2, |cols| {
                            cols[0].vertical(|ui| {
                                ui.strong("Before");
                                egui::ScrollArea::vertical()
                                    .id_salt("diff_before")
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        let before_text = extract(before, path)
                                            .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                                            .unwrap_or_default();
                                        ui.label(egui::RichText::new(&before_text).monospace());
                                    });
                            });
                            cols[1].vertical(|ui| {
                                ui.strong("After");
                                egui::ScrollArea::vertical()
                                    .id_salt("diff_after")
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        let after_text = extract(after, path)
                                            .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                                            .unwrap_or_default();
                                        ui.label(egui::RichText::new(&after_text).monospace());
                                    });
                            });
                        });
                    }
                } else {
                    ui.label("select a node above");
                }
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            let mut nav: i64 = 0;
            ui.horizontal(|ui| {
                if ui.button("<< Prev").clicked() {
                    nav = -1;
                }
                if ui.button("Next >>").clicked() {
                    nav = 1;
                }
                ui.separator();
                ui.label(&self.status_message);
            });
            if nav != 0 {
                self.navigate(nav);
            }
            ui.separator();

            egui::ScrollArea::vertical()
                .id_salt("tree")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    show_tree(ui, &self.tree, &mut self.selected_path);
                });
        });
    }
}
