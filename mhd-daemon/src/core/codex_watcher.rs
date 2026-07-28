//! Background Codex telemetry watcher.
//!
//! Runs the JSONL import + live API fetch on a periodic timer inside the
//! daemon process. Toggle on/off via tray or action binding.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mhd_telemetry::codex;
use mhd_telemetry::db;
use mhd_telemetry::import;
use mhd_telemetry::live;

/// Whether the watcher thread is currently running.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Join handle of the running watcher thread.
static THREAD: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

/// Whether the watcher is currently running.
pub fn is_running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

/// Start the background watcher thread. No-op if already running.
pub fn start() -> bool {
    if RUNNING.load(Ordering::SeqCst) {
        return true;
    }

    RUNNING.store(true, Ordering::SeqCst);

    let handle = std::thread::spawn(move || {
        run_loop();
    });

    if let Ok(mut guard) = THREAD.lock() {
        *guard = Some(handle);
    }

    println!("mhd: codex-watcher started");
    true
}

/// Stop the background watcher thread.
pub fn stop() {
    RUNNING.store(false, Ordering::SeqCst);

    if let Ok(mut guard) = THREAD.lock() {
        if let Some(handle) = guard.take() {
            let _ = handle.join();
        }
    }

    println!("mhd: codex-watcher stopped");
}

/// Toggle the watcher on/off. Returns the new state.
pub fn toggle() -> bool {
    if is_running() {
        stop();
        false
    } else {
        start()
    }
}

/// The watcher loop: import JSONL → fetch live API → store → sleep
fn run_loop() {
    let db_path = db::default_db_path();
    let codex_home = codex::codex_home();

    while RUNNING.load(Ordering::SeqCst) {
        // Open DB (create if missing, with migration)
        if let Ok(mut telemetry_db) = db::open_or_create(&db_path) {
            // Step 1: import new JSONL rows
            let _result = import::run_import(&mut telemetry_db, &codex_home);

            // Step 2: fetch live API quota and store it
            if let Ok(lq) = live::fetch_live_quota(&codex_home) {
                if let Some(ref s) = lq.session {
                    let _ = telemetry_db.store_live_snapshot(
                        "codex",
                        "5h",
                        300,
                        s.used_percent,
                        s.resets_at,
                        lq.plan_type.as_deref(),
                    );
                }
                if let Some(ref w) = lq.weekly {
                    let _ = telemetry_db.store_live_snapshot(
                        "codex",
                        "7d",
                        10080,
                        w.used_percent,
                        w.resets_at,
                        lq.plan_type.as_deref(),
                    );
                }
            }
        } else if RUNNING.load(Ordering::SeqCst) {
            // DB open failed; wait before retry
            std::thread::sleep(Duration::from_secs(30));
            continue;
        }

        // Wait for next cycle (check RUNNING periodically)
        for _ in 0..30 {
            if !RUNNING.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}
