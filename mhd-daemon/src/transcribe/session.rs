//! Transcribe session state machine.
//!
//! A session starts when the user presses the transcribe hotkey and
//! ends when they press it again (or on error/resources exhausted).

use crate::transcribe::config::TranscribeConfig;

/// Possible states of a transcribe session.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SessionState {
    Idle,
    Starting,
    Recording,
    Stopping,
    Finalizing,
    Outputting,
    Done,
    Error,
}

impl SessionState {
    pub fn is_active(self) -> bool {
        matches!(self, SessionState::Starting | SessionState::Recording | SessionState::Stopping | SessionState::Finalizing | SessionState::Outputting)
    }
}

/// A handle to an ongoing transcribe session.
pub struct TranscribeSession {
    state: SessionState,
    config: TranscribeConfig,
    /// Accumulated final text.
    result: String,
    /// Any error message.
    error: Option<String>,
}

impl TranscribeSession {
    /// Create a new idle session.
    pub fn new(config: TranscribeConfig) -> Self {
        TranscribeSession {
            state: SessionState::Idle,
            config,
            result: String::new(),
            error: None,
        }
    }

    pub fn state(&self) -> SessionState {
        self.state
    }

    pub fn result(&self) -> &str {
        &self.result
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Start a session. For Phase 1, this transcribes a test WAV file.
    /// In later phases this will start microphone capture.
    pub fn start(&mut self) -> Result<(), String> {
        self.state = SessionState::Starting;
        // Phase 1: no-op (will be implemented with microphone capture in Phase 2)
        self.state = SessionState::Recording;
        Ok(())
    }

    /// Stop recording and finalize transcription.
    pub fn stop(&mut self) -> Result<(), String> {
        self.state = SessionState::Stopping;
        self.state = SessionState::Finalizing;
        // Phase 1: no-op (transcription happens synchronously in start for now)
        self.state = SessionState::Done;
        Ok(())
    }

    /// Reset to idle.
    pub fn reset(&mut self) {
        self.state = SessionState::Idle;
        self.result.clear();
        self.error = None;
    }
}
