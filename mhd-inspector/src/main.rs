//! mhd-inspector — Context Trim inspector and LLM Monitor.
//!
//! Modes:
//! - Default / with --db, --id, --run-id, --seq: standalone Context Trim inspector.
//! - With --monitor flag: tabbed LLM Monitor (Overview / Activity / Context Trim).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod activity;
mod anthropic_quota;
mod app;
mod context_trim;
mod overview;
mod timefmt;

use std::path::PathBuf;

use eframe::egui;

use app::MonitorApp;
use context_trim::ContextTrimApp;

// ── CLI args (hand-rolled) ───────────────────────────────────────────────────

struct CliArgs {
    db_path: Option<String>,
    row_id: Option<i64>,
    run_id: Option<i64>,
    seq: Option<i64>,
    monitor: bool,
}

fn parse_args() -> CliArgs {
    let raw: Vec<String> = std::env::args().collect();
    let mut args = CliArgs {
        db_path: None,
        row_id: None,
        run_id: None,
        seq: None,
        monitor: false,
    };

    let mut i = 1;
    while i < raw.len() {
        match raw[i].as_str() {
            "--db" => {
                i += 1;
                if i < raw.len() {
                    args.db_path = Some(raw[i].clone());
                }
            }
            "--id" => {
                i += 1;
                if i < raw.len() {
                    args.row_id = raw[i].parse::<i64>().ok();
                }
            }
            "--run-id" => {
                i += 1;
                if i < raw.len() {
                    args.run_id = raw[i].parse::<i64>().ok();
                }
            }
            "--seq" => {
                i += 1;
                if i < raw.len() {
                    args.seq = raw[i].parse::<i64>().ok();
                }
            }
            "--monitor" => {
                args.monitor = true;
            }
            _ => {}
        }
        i += 1;
    }
    args
}

// ── DB path resolution (used in ContextTrim mode) ───────────────────────────

fn resolve_db_path(cli: &CliArgs) -> PathBuf {
    if let Some(ref p) = cli.db_path {
        return PathBuf::from(p);
    }
    let home = dirs::home_dir().unwrap_or_default();
    let p1 = home
        .join(".config")
        .join("mhd")
        .join("llm-proxy")
        .join("proxy.db");
    if p1.exists() {
        return p1;
    }
    let config = dirs::config_dir().unwrap_or_default();
    let p2 = config.join("mhd").join("llm-proxy").join("proxy.db");
    if p2.exists() {
        return p2;
    }
    p1
}

// ── Entry point ──────────────────────────────────────────────────────────────

fn main() -> eframe::Result<()> {
    let cli = parse_args();

    if cli.monitor {
        // LLM Monitor mode
        eframe::run_native(
            "LLM Monitor",
            eframe::NativeOptions {
                viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 760.0]),
                ..Default::default()
            },
            Box::new(|cc| Ok(Box::new(MonitorApp::new(cc)))),
        )
    } else {
        // Context Trim mode (original behavior)
        let db_path = resolve_db_path(&cli);
        eframe::run_native(
            "mhd-inspector",
            eframe::NativeOptions {
                viewport: egui::ViewportBuilder::default().with_inner_size([1000.0, 700.0]),
                ..Default::default()
            },
            Box::new(|cc| {
                Ok(Box::new(ContextTrimApp::new(
                    cc, db_path, cli.row_id, cli.run_id, cli.seq,
                )))
            }),
        )
    }
}
