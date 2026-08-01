//! LLM Monitor — tabbed window for observing LLM usage and agent activity.
//!
//! Tabs: Progress (quota + trajectory) and Request history (model calls).

use eframe::egui;
use mhd_telemetry::codex;
use mhd_telemetry::db::{self, TelemetryDb};
use mhd_telemetry::import;
use mhd_telemetry::live;
use mhd_telemetry::query::{self, ActivityRow, QuotaSample, SlopeProjection};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::activity;
use crate::anthropic_quota;
use crate::overview;
use crate::timefmt;

/// Tabs in the monitor window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Activity,
}

/// Chart span: the decision view (current cycle) or several cycles back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChartRange {
    Cycle,
    History,
}

impl ChartRange {
    fn label(&self) -> &str {
        match self {
            ChartRange::Cycle => "Cycle",
            ChartRange::History => "History",
        }
    }
}

/// State for the complete LLM Monitor window.
pub struct MonitorApp {
    // ── Tabs ──
    active_tab: Tab,

    // ── Controls ──
    provider: String,
    project_filter: Option<String>,
    period: Period,
    chart_range: ChartRange,
    last_refresh: Instant,
    status_message: String,
    note_open: bool,
    note_text: String,
    note_status: Option<String>,

    // ── Telemetry ──
    db: Option<TelemetryDb>,
    codex_home: PathBuf,
    import_result: Option<import::ImportResult>,
    latest_sample: Option<i64>,

    // ── Overview data ──
    quota_5h: Option<query::Utilization>,
    quota_7d: Option<query::Utilization>,
    slope_proj_5h: SlopeProjection,
    slope_proj_7d: SlopeProjection,
    quota_history: Vec<QuotaSample>,
    quota_cycle_starts: Vec<i64>,
    local_offset: i64,
    live_quota: Option<live::LiveQuota>,
    last_live_refresh: Instant,

    // ── Activity data ──
    activity_rows: Vec<ActivityRow>,
    projects: Vec<String>,

    // ── Anthropic quota (from proxy.db) ──
    anthropic_quota: Option<anthropic_quota::AnthropicQuota>,
    anthropic_history: Vec<anthropic_quota::QuotaPoint>,
    last_anthropic_refresh: Instant,

    // ── Import thread ──
    #[allow(dead_code)]
    import_tx: mpsc::Sender<import::ImportResult>,
    #[allow(dead_code)]
    import_rx: mpsc::Receiver<import::ImportResult>,
}

/// Time period selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Period {
    SixHours,
    TwentyFourHours,
    SevenDays,
    CurrentCycle,
    All,
}

impl Period {
    fn label(&self) -> &str {
        match self {
            Period::SixHours => "6h",
            Period::TwentyFourHours => "24h",
            Period::SevenDays => "7d",
            Period::CurrentCycle => "Current cycle",
            Period::All => "All",
        }
    }

    fn since_epoch(&self) -> i64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        match self {
            Period::SixHours => now - 6 * 3600,
            Period::TwentyFourHours => now - 24 * 3600,
            Period::SevenDays => now - 7 * 24 * 3600,
            Period::CurrentCycle | Period::All => 0,
        }
    }
}

impl MonitorApp {
    /// Create a new monitor app. Initialises the telemetry DB and runs the first import.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());

        // ── Telemetry DB ──
        let db_path = db::default_db_path();
        let db = db::open_or_create(&db_path).ok();

        let codex_home = codex::codex_home();

        // ── Import channel ──
        let (import_tx, import_rx) = mpsc::channel::<import::ImportResult>();

        let mut app = MonitorApp {
            active_tab: Tab::Overview,
            provider: "codex".to_string(),
            project_filter: None,
            period: Period::All,
            chart_range: ChartRange::Cycle,
            last_refresh: Instant::now(),
            status_message: String::new(),
            note_open: false,
            note_text: String::new(),
            note_status: None,
            db,
            codex_home,
            import_result: None,
            latest_sample: None,
            quota_5h: None,
            quota_7d: None,
            slope_proj_5h: SlopeProjection::default(),
            slope_proj_7d: SlopeProjection::default(),
            quota_history: Vec::new(),
            quota_cycle_starts: Vec::new(),
            local_offset: timefmt::local_offset_seconds(),
            live_quota: None,
            last_live_refresh: Instant::now() - Duration::from_secs(60), // prompt first refresh
            activity_rows: Vec::new(),
            projects: Vec::new(),
            anthropic_quota: None,
            anthropic_history: Vec::new(),
            last_anthropic_refresh: Instant::now() - Duration::from_secs(60),
            import_tx,
            import_rx,
        };

        app.refresh_data();
        app
    }

    /// Refresh all data from the telemetry DB.
    fn refresh_data(&mut self) {
        let Some(ref db) = self.db else {
            self.status_message = "telemetry.db not available".into();
            return;
        };

        // Projects
        self.projects = query::list_projects(db);

        // Quota — prefer live data, fall back to DB
        if let Some(ref lq) = self.live_quota {
            self.quota_5h = lq.session.clone().map(query::Utilization::from);
            self.quota_7d = lq.weekly.clone().map(query::Utilization::from);
        } else {
            self.quota_5h = query::current_utilization(db, &self.provider, "5h");
            self.quota_7d = query::current_utilization(db, &self.provider, "7d");
        }

        // Slopes
        self.slope_proj_5h = query::slope_and_projection(db, &self.provider, "5h");
        self.slope_proj_7d = query::slope_and_projection(db, &self.provider, "7d");

        // Chart the budget the account actually has. Pro 5x exposes a weekly
        // window only, while other plans can still fall back to the 5h window.
        let (window_kind, window_seconds, resets_at) = if let Some(weekly) = &self.quota_7d {
            ("7d", 7 * 24 * 3600, weekly.resets_at)
        } else if let Some(session) = &self.quota_5h {
            ("5h", 5 * 3600, session.resets_at)
        } else {
            ("7d", 7 * 24 * 3600, None)
        };
        let current_cycle_since = query::current_cycle_start(db, &self.provider, window_kind)
            .unwrap_or_else(|| {
                resets_at
                    .map(|reset| reset.saturating_sub(window_seconds))
                    .unwrap_or(0)
            });
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // History spans four cycles so past resets are visible on the X axis.
        let history_since = now - window_seconds * 4;
        let chart_since = if self.chart_range == ChartRange::History {
            history_since
        } else {
            resets_at
                .map(|reset| reset.saturating_sub(window_seconds))
                .unwrap_or(current_cycle_since)
        };
        self.quota_history = query::quota_samples(db, &self.provider, window_kind, chart_since);
        self.quota_cycle_starts = query::cycle_boundaries(db, &self.provider, window_kind);

        // "Current cycle" follows the same provider-reported quota window as
        // the progress chart, rather than silently meaning all history.
        let since = if self.period == Period::CurrentCycle {
            current_cycle_since
        } else {
            self.period.since_epoch()
        };

        // Activity rows
        self.activity_rows =
            query::model_calls_list(db, since, self.project_filter.as_deref(), 500, 0);

        // Latest sample time
        self.latest_sample = query::latest_sample_time(db);

        // Anthropic quota from proxy.db (refresh on the 10s cadence, not every refresh_data)
        if self.last_anthropic_refresh.elapsed() >= Duration::from_secs(10) {
            self.refresh_anthropic_quota();
        }

        // Status — prefer live data info when available
        self.status_message = {
            let mut parts = Vec::new();
            if let Some(ref lq) = self.live_quota {
                let plan = live::display_plan_type(lq.plan_type.as_deref());
                parts.push(plan.to_string());
                if let Some(session) = &lq.session {
                    parts.push(format!("5h {:.0}%", session.used_percent));
                }
                if let Some(weekly) = &lq.weekly {
                    parts.push(format!("7d {:.0}%", weekly.used_percent));
                }
            } else if let Some(latest_sample) = self.latest_sample {
                parts.push(format!("Last sample: {}", relative_time(latest_sample)));
            } else {
                parts.push("No data imported yet".into());
            }
            if let Some(ref aq) = self.anthropic_quota {
                let mut anthropic_parts = vec!["Anthropic".to_string()];
                if let Some(h5) = aq.h5 {
                    anthropic_parts.push(format!("5h {:.0}%", h5));
                }
                if let Some(d7) = aq.d7 {
                    anthropic_parts.push(format!("7d {:.0}%", d7));
                }
                parts.push(anthropic_parts.join(" "));
            }
            parts.join(" | ")
        };
    }

    /// Fetch live quota from the Codex backend API.
    fn refresh_live_quota(&mut self) {
        self.live_quota = live::fetch_live_quota(&self.codex_home).ok();
        self.last_live_refresh = Instant::now();
        self.refresh_data();
    }

    /// Read the latest Anthropic quota snapshot and sparkline history from proxy.db.
    fn refresh_anthropic_quota(&mut self) {
        let db_path = llm_proxy::config::config_dir().join("proxy.db");
        self.anthropic_quota = anthropic_quota::read_latest(&db_path);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // Bounding the query by time keeps the 10s refresh affordable; history
        // needs four cycles back, the decision view only a week.
        let (since, limit) = match self.chart_range {
            ChartRange::History => (now - 28 * 24 * 3600, 20000),
            ChartRange::Cycle => (now - 8 * 24 * 3600, 5000),
        };
        self.anthropic_history = anthropic_quota::read_history_since(&db_path, since, limit);
        self.last_anthropic_refresh = Instant::now();
    }

    /// Run an incremental import pass.
    fn run_import(&mut self) {
        let Some(ref mut db) = self.db else { return };
        let result = import::run_import(db, &self.codex_home);
        self.import_result = Some(result);
        self.refresh_data();
    }
}

impl eframe::App for MonitorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Title("LLM Monitor".to_string()));

        // ── Periodic refresh (import) ──
        if self.last_refresh.elapsed() >= Duration::from_secs(5) {
            self.run_import();
            self.last_refresh = Instant::now();
        }

        // ── Live quota refresh (30s interval) ──
        if self.last_live_refresh.elapsed() >= Duration::from_secs(30) {
            self.refresh_live_quota();
        }

        // ── Top bar: global controls ──
        egui::TopBottomPanel::top("controls").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Provider:");
                if ui
                    .selectable_label(self.provider == "codex", "Codex")
                    .clicked()
                {
                    self.provider = "codex".to_string();
                    self.refresh_data();
                }
                if ui
                    .selectable_label(self.provider == "anthropic", "Anthropic")
                    .clicked()
                {
                    self.provider = "anthropic".to_string();
                    self.refresh_data();
                }

                if self.active_tab == Tab::Activity {
                    ui.separator();
                    ui.label("Project:");
                    if ui
                        .selectable_label(self.project_filter.is_none(), "All")
                        .clicked()
                    {
                        self.project_filter = None;
                        self.refresh_data();
                    }
                    let projects_clone = self.projects.clone();
                    for p in &projects_clone {
                        let is_selected = self.project_filter.as_deref() == Some(p);
                        if ui.selectable_label(is_selected, p).clicked() {
                            self.project_filter = Some(p.clone());
                            self.refresh_data();
                        }
                    }

                    ui.separator();
                    ui.label("Period:");
                    for p in &[
                        Period::SixHours,
                        Period::TwentyFourHours,
                        Period::SevenDays,
                        Period::CurrentCycle,
                        Period::All,
                    ] {
                        if ui.selectable_label(self.period == *p, p.label()).clicked() {
                            self.period = *p;
                            self.refresh_data();
                        }
                    }
                }

                if self.active_tab == Tab::Overview {
                    ui.separator();
                    ui.label("Range:");
                    for variant in [ChartRange::Cycle, ChartRange::History] {
                        if ui
                            .selectable_label(self.chart_range == variant, variant.label())
                            .clicked()
                        {
                            self.chart_range = variant;
                            self.refresh_data();
                        }
                    }
                }

                ui.separator();

                if ui.button("Refresh").clicked() {
                    self.run_import();
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(&self.status_message);
                    if ui.button("Note").clicked() {
                        self.note_open = true;
                        self.note_status = None;
                    }
                });
            });
        });

        // ── Quick Note modal ──
        if self.note_open {
            let mut open = self.note_open;
            let mut save = false;
            let mut cancel = false;
            egui::Window::new("Quick Note")
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label("Add a marker to the Codex quota timeline:");
                    let edit = ui.add_sized(
                        [360.0, 90.0],
                        egui::TextEdit::multiline(&mut self.note_text)
                            .hint_text("Changed account / upgraded to Pro…"),
                    );
                    edit.request_focus();
                    if let Some(status) = &self.note_status {
                        ui.colored_label(egui::Color32::LIGHT_RED, status);
                    }
                    ui.horizontal(|ui| {
                        let can_save = !self.note_text.trim().is_empty();
                        if ui
                            .add_enabled(can_save, egui::Button::new("Save"))
                            .clicked()
                        {
                            save = true;
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                });

            if save {
                let plan = self
                    .live_quota
                    .as_ref()
                    .and_then(|q| q.plan_type.as_deref());
                match self
                    .db
                    .as_ref()
                    .ok_or("telemetry.db not available")
                    .and_then(|db| {
                        db.insert_note(Some(&self.provider), plan, self.note_text.trim())
                            .map_err(|_| "Could not save note")
                    }) {
                    Ok(()) => {
                        self.status_message = "Note saved".into();
                        self.note_text.clear();
                        self.note_status = None;
                        open = false;
                    }
                    Err(message) => self.note_status = Some(message.into()),
                }
            }
            if cancel {
                self.note_text.clear();
                self.note_status = None;
                open = false;
            }
            self.note_open = open;
        }

        // ── Tab bar ──
        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.active_tab, Tab::Overview, "Progress");
                ui.selectable_value(&mut self.active_tab, Tab::Activity, "Request history");
            });
        });

        // ── Tab content ──
        egui::CentralPanel::default().show(ctx, |ui| match self.active_tab {
            Tab::Overview => {
                overview::show_overview(
                    ui,
                    &self.provider,
                    &self.quota_5h,
                    &self.quota_7d,
                    &self.slope_proj_5h,
                    &self.slope_proj_7d,
                    &self.quota_history,
                    &self.import_result,
                    self.live_quota.as_ref(),
                    self.anthropic_quota.as_ref(),
                    &self.anthropic_history,
                    &self.quota_cycle_starts,
                    overview::ChartOpts {
                        local_offset: self.local_offset,
                        history: self.chart_range == ChartRange::History,
                    },
                    ctx,
                );
            }
            Tab::Activity => {
                activity::show_activity(
                    ui,
                    &self.activity_rows,
                    &self.projects,
                    &self.project_filter,
                );
            }
        });

        // Request repaint every 5s for periodic refresh
        ctx.request_repaint_after(Duration::from_secs(5));
    }
}

/// Format a unix timestamp as a relative time string.
pub(crate) fn relative_time(ts: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let delta = now - ts;
    if delta < 60 {
        format!("{delta}s ago")
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86400 {
        format!("{}h {}m ago", delta / 3600, (delta % 3600) / 60)
    } else {
        format!("{}d ago", delta / 86400)
    }
}
