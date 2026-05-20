//! App — core orchestration for mhd.
//!
//! Owns config, worker, hooks, OSD, and IPC. Provides [`AppHandle`] for
//! external control (tray module, IPC commands).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW;
use windows::Win32::UI::WindowsAndMessaging::WM_QUIT;

use crate::config::AppConfig;
use crate::native_theme::NativeTheme;
use crate::osd::OsdHandle;
use crate::worker::{ActionSender, ActionWorker};

/// Wrapper to make HWND Send+Sync safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendHwnd(pub HWND);
unsafe impl Send for SendHwnd {}
unsafe impl Sync for SendHwnd {}

/// Handle to a running [`App`], usable from tray or IPC.
///
/// All methods are synchronous and don't require a named pipe.
#[derive(Clone)]
pub struct AppHandle {
    pub(crate) running: Arc<AtomicBool>,
    pub(crate) config: Arc<Mutex<AppConfig>>,
    pub(crate) config_path: PathBuf,
    /// Thread ID of the hook message loop. Set inside [`App::run`].
    /// IPC/tray post WM_QUIT to this thread on shutdown.
    pub(crate) hook_thread_id: Arc<AtomicU32>,
    pub(crate) quiet: bool,
    pub(crate) theme: Arc<Mutex<NativeTheme>>,
    pub(crate) recording_window: Arc<Mutex<Option<SendHwnd>>>,
    osd: OsdHandle,
}

impl AppHandle {
    /// Get the current theme.
    pub fn theme(&self) -> NativeTheme {
        self.theme.lock().unwrap().clone()
    }

    /// Check whether the daemon core is still running.
    pub fn status(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Re-read the config file and rebuild the trigger map.
    ///
    /// The hook callbacks will see the new config on the next trigger.
    pub fn reload_config(&self) -> Result<(), String> {
        let content = std::fs::read_to_string(&self.config_path)
            .map_err(|e| format!("cannot read config: {e}"))?;
        let new_config = AppConfig::parse(&content, &self.config_path)?;

        let new_theme = crate::native_theme::load_theme(new_config.theme.as_deref());

        let bindings_count = new_config.active_bindings().len();

        // Update theme first, then config
        {
            let mut theme = self.theme.lock().unwrap();
            *theme = new_theme;
        }
        {
            let mut config = self.config.lock().unwrap();
            *config = new_config;
        }

        // Push theme to OSD
        self.osd.set_theme(self.theme());

        if !self.quiet {
            println!("mhd: config reloaded ({bindings_count} bindings)");
        }
        Ok(())
    }

    /// Signal the daemon core to shut down gracefully.
    ///
    /// Posts `WM_QUIT` to the hook message loop thread and sets the
    /// running flag to `false`.
    pub fn shutdown(&self) {
        self.running.store(false, Ordering::SeqCst);
        let tid = self.hook_thread_id.load(Ordering::SeqCst);
        if tid != 0 {
            unsafe {
                let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
    }
}

/// The mhd application core.
///
/// Create with [`App::new`], obtain an [`AppHandle`] via [`App::handle`],
/// then call [`App::run`] to enter the hook message loop (blocking).
pub struct App {
    config: Arc<Mutex<AppConfig>>,
    config_path: PathBuf,
    running: Arc<AtomicBool>,
    hook_thread_id: Arc<AtomicU32>,
    quiet: bool,
    tx: ActionSender,
    osd: OsdHandle,
    theme: Arc<Mutex<NativeTheme>>,
    recording_window: Arc<Mutex<Option<SendHwnd>>>,
}

impl App {
    /// Parse the config file and create the worker thread.
    ///
    /// Returns an error if the config is missing, unparseable, or has no
    /// active bindings.
    pub fn new(config_path: PathBuf, quiet: bool, osd: OsdHandle) -> Result<Self, String> {
        let content = std::fs::read_to_string(&config_path)
            .map_err(|e| format!("cannot read config: {e}"))?;
        let app_config = AppConfig::parse(&content, &config_path)?;

        if app_config.active_bindings().is_empty() {
            return Err(format!("config empty: {}", config_path.display()));
        }

        let native_theme = crate::native_theme::load_theme(app_config.theme.as_deref());

        let (worker, tx) = ActionWorker::new(quiet, osd.clone());
        // Worker thread runs until the channel closes (when `tx` is dropped).
        let _worker_handle = worker.spawn();

        let running = Arc::new(AtomicBool::new(true));
        let hook_thread_id = Arc::new(AtomicU32::new(0));

        if !quiet {
            println!("mhd: loaded config: {}", config_path.display());
            println!(
                "mhd: loaded bindings: {}",
                app_config.active_bindings().len()
            );
        }

        Ok(App {
            config: Arc::new(Mutex::new(app_config)),
            config_path,
            running,
            hook_thread_id,
            quiet,
            tx,
            osd,
            theme: Arc::new(Mutex::new(native_theme)),
            recording_window: Arc::new(Mutex::new(None)),
        })
    }

    /// Create an [`AppHandle`] that can be shared with tray/IPC **before**
    /// [`App::run`] is called.
    ///
    /// The hook thread ID is set inside `run`; any `shutdown` call before
    /// that is a no-op (which is fine — the process hasn't started yet).
    pub fn handle(&self) -> AppHandle {
        AppHandle {
            running: self.running.clone(),
            config: self.config.clone(),
            config_path: self.config_path.clone(),
            hook_thread_id: self.hook_thread_id.clone(),
            quiet: self.quiet,
            osd: self.osd.clone(),
            theme: self.theme.clone(),
            recording_window: self.recording_window.clone(),
        }
    }

    /// Install low-level hooks and enter the blocking message loop.
    ///
    /// Returns when `WM_QUIT` is received (from IPC/tray shutdown) or on
    /// hook installation error.
    pub fn run(self) -> Result<(), String> {
        // Record the thread ID – this is where hooks + message loop live.
        let tid = unsafe { GetCurrentThreadId() };
        self.hook_thread_id.store(tid, Ordering::SeqCst);
        let handle = self.handle();

        crate::hook::run_with_config(handle, self.tx)
    }
}
