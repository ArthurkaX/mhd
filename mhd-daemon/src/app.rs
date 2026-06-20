//! App — core orchestration for mhd.
//!
//! Owns config, worker, hooks, OSD, and IPC. Provides [`AppHandle`] for
//! external control (tray module, IPC commands).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};

#[cfg(feature = "blackbox")]
use crate::blackbox::{BlackboxConfig, BlackboxHandle};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW;
use windows::Win32::UI::WindowsAndMessaging::WM_QUIT;

use crate::action::Action;
use crate::config::AppConfig;
use crate::native_theme::NativeTheme;
use crate::osd::OsdHandle;
use crate::overlays::note::QuickNoteConfig;
use crate::trigger::Trigger;
use crate::worker::{ActionSender, ActionWorker};

/// Interface for controlling the daemon and accessing its state.
pub trait DaemonControl: Send + Sync {
    fn shutdown(&self);
    fn reload_config(&self) -> Result<(), String>;
    fn status(&self) -> bool;
    fn theme(&self) -> NativeTheme;
    fn active_scheme(&self) -> String;
    fn switch_scheme(&self, name: &str) -> bool;
    fn lookup_trigger(&self, trigger: &Trigger) -> Option<Action>;
    fn quiet(&self) -> bool;
    #[allow(dead_code)]
    fn config_path(&self) -> &std::path::Path;
    /// Toggle blackbox logging on/off.
    #[cfg(feature = "blackbox")]
    fn toggle_blackbox(&self);
    /// Whether blackbox is currently running.
    #[cfg(feature = "blackbox")]
    fn blackbox_enabled(&self) -> bool;
    /// Quick Note config snapshot.
    fn quicknote_config(&self) -> QuickNoteConfig;
    /// Quick Draw save directory.
    fn draw_dir(&self) -> std::path::PathBuf;
    /// KeyCast config snapshot.
    fn keycast_config(&self) -> crate::overlays::keycast::KeycastConfig;
    /// LLM proxy config snapshot.
    fn llm_proxy_config(&self) -> crate::config::LlmProxyConfig;
}

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
    pub(crate) osd: OsdHandle,
    /// Blackbox handle (for tray toggle).
    #[cfg(feature = "blackbox")]
    pub(crate) blackbox: Arc<Mutex<Option<BlackboxHandle>>>,
}

impl DaemonControl for AppHandle {
    fn theme(&self) -> NativeTheme {
        self.theme.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn status(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    fn reload_config(&self) -> Result<(), String> {
        let content = std::fs::read_to_string(&self.config_path)
            .map_err(|e| format!("cannot read config: {e}"))?;
        let new_config = AppConfig::parse(&content, &self.config_path)?;

        let new_theme = crate::native_theme::load_theme(new_config.theme.as_deref());
        let bindings_count = new_config.active_bindings().len();

        {
            let mut theme = self.theme.lock().unwrap_or_else(|e| e.into_inner());
            *theme = new_theme;
        }
        {
            let mut config = self.config.lock().unwrap_or_else(|e| e.into_inner());
            *config = new_config;
        }

        self.osd.set_theme(self.theme());

        if !self.quiet {
            println!("mhd: config reloaded ({bindings_count} bindings)");
        }
        Ok(())
    }

    fn shutdown(&self) {
        self.running.store(false, Ordering::SeqCst);
        let tid = self.hook_thread_id.load(Ordering::SeqCst);
        if tid != 0 {
            unsafe {
                let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
    }

    fn switch_scheme(&self, name: &str) -> bool {
        // Read the theme name under the lock, then release it BEFORE the disk
        // I/O in `load_theme`. The keyboard hot path also locks `config`
        // (`lookup_trigger`), so holding it across disk I/O can stall keystrokes
        // long enough to trip the low-level-hook timeout.
        let theme_name = {
            let mut config = self.config.lock().unwrap_or_else(|e| e.into_inner());
            if !config.switch_scheme(name) {
                return false;
            }
            config.theme.clone()
        };
        let new_theme = crate::native_theme::load_theme(theme_name.as_deref());
        {
            let mut theme = self.theme.lock().unwrap_or_else(|e| e.into_inner());
            *theme = new_theme;
        }
        self.osd.set_theme(self.theme());
        true
    }

    fn active_scheme(&self) -> String {
        self.config.lock().unwrap_or_else(|e| e.into_inner()).active_scheme().to_string()
    }

    fn lookup_trigger(&self, trigger: &Trigger) -> Option<Action> {
        let config = self.config.lock().unwrap_or_else(|e| e.into_inner());
        config.lookup_trigger(trigger).map(|b| b.action.clone())
    }

    fn quiet(&self) -> bool {
        self.quiet
    }

    fn config_path(&self) -> &std::path::Path {
        &self.config_path
    }

    #[cfg(feature = "blackbox")]
    fn toggle_blackbox(&self) {
        if let Ok(guard) = self.blackbox.lock() {
            if let Some(ref bb) = *guard {
                bb.toggle();
            }
        }
    }

    #[cfg(feature = "blackbox")]
    fn blackbox_enabled(&self) -> bool {
        crate::blackbox::is_logging()
    }

    fn quicknote_config(&self) -> QuickNoteConfig {
        self.config.lock().unwrap_or_else(|e| e.into_inner()).quicknote_config().clone()
    }

    fn draw_dir(&self) -> std::path::PathBuf {
        self.config.lock().unwrap_or_else(|e| e.into_inner()).draw_dir().clone()
    }

    fn keycast_config(&self) -> crate::overlays::keycast::KeycastConfig {
        self.config.lock().unwrap_or_else(|e| e.into_inner()).keycast_config().clone()
    }

    fn llm_proxy_config(&self) -> crate::config::LlmProxyConfig {
        self.config.lock().unwrap_or_else(|e| e.into_inner()).llm_proxy().clone()
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
    #[cfg(feature = "blackbox")]
    blackbox: Arc<Mutex<Option<BlackboxHandle>>>,
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

        let running = Arc::new(AtomicBool::new(true));
        let hook_thread_id = Arc::new(AtomicU32::new(0));
        let config = Arc::new(Mutex::new(app_config));
        let theme = Arc::new(Mutex::new(native_theme));
        #[cfg(feature = "blackbox")]
        let blackbox = Arc::new(Mutex::new(None::<BlackboxHandle>));

        let handle = AppHandle {
            running: running.clone(),
            config: config.clone(),
            config_path: config_path.clone(),
            hook_thread_id: hook_thread_id.clone(),
            quiet,
            osd: osd.clone(),
            theme: theme.clone(),
            #[cfg(feature = "blackbox")]
            blackbox: blackbox.clone(),
        };

        let (worker, tx) = ActionWorker::new(handle);
        // Worker thread runs until the channel closes (when `tx` is dropped).
        let _worker_handle = worker.spawn();

        if !quiet {
            println!("mhd: loaded config: {}", config_path.display());
            println!(
                "mhd: loaded bindings: {}",
                config.lock().unwrap().active_bindings().len()
            );
        }

        Ok(App {
            config,
            config_path,
            running,
            hook_thread_id,
            quiet,
            tx,
            osd,
            theme,
            #[cfg(feature = "blackbox")]
            blackbox,
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
            #[cfg(feature = "blackbox")]
            blackbox: self.blackbox.clone(),
        }
    }

    /// Start the blackbox monitoring worker if enabled in config.
    #[cfg(feature = "blackbox")]
    fn start_blackbox(config: &BlackboxConfig) -> Option<BlackboxHandle> {
        if !config.enabled {
            return None;
        }
        match crate::blackbox::start(config.clone()) {
            Ok(h) => Some(h),
            Err(e) => {
                eprintln!("mhd: blackbox: {e}");
                None
            }
        }
    }

    pub fn run(self) -> Result<(), String> {
        let tid = unsafe { GetCurrentThreadId() };
        self.hook_thread_id.store(tid, Ordering::SeqCst);

        // Start blackbox if configured
        #[cfg(feature = "blackbox")]
        {
            let config = self.config.lock().unwrap_or_else(|e| e.into_inner());
            let bb_config = config.blackbox().clone();
            if bb_config.enabled {
                if let Some(h) = Self::start_blackbox(&bb_config) {
                    *self.blackbox.lock().unwrap() = Some(h);
                }
            }
        }

        let handle = self.handle();
        let result = crate::hook::run_with_config(handle, self.tx);

        // Shutdown blackbox after hook exits
        #[cfg(feature = "blackbox")]
        if let Some(mut bb) = self.blackbox.lock().unwrap().take() {
            bb.shutdown();
        }

        result
    }
}
