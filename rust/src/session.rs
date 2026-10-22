//! Transcript state machine of one recognition session, driven by
//! `coordinator::run_session`. Pure data: no I/O, no timers, which is why
//! `tests/session_finalize.rs` exercises it directly.
//!
//! It folds backend events (`realtime::RealtimeEvent`, identical for cloud
//! and local backends) into a `TranscriptSnapshot`, tracks the lifecycle
//! `Recording -> Stopping -> Completed | Cancelled | Failed`, and decides
//! whether a stop request means "finish" or "cancel". The injection text
//! exists only after a `Finished` event received while `Stopping`, and
//! `take_injection_text` hands it out once: that is what lets the
//! coordinator inject at most once and never on cancel or error paths.

/// Immutable view of the transcript at one point in time; serialized as
/// the `"transcript"` object of `stream --json` lines and rendered by the
/// console and the Fcitx5 preedit.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TranscriptSnapshot {
    /// Segments the backend confirmed, in order (a segment repeating the
    /// previous one is stored once).
    pub confirmed_segments: Vec<String>,
    /// Current unconfirmed text (the backend's partial plus its stash);
    /// empty in a final snapshot.
    pub partial_text: String,
    /// Best text so far: confirmed segments followed by the partial. In a
    /// final snapshot, the backend's full transcript, or the joined
    /// segments when the backend returned an empty one.
    pub authoritative_text: String,
    /// `true` only for the single snapshot produced by `Finished`.
    pub is_final: bool,
    /// Backend id the session was created with (`SessionPlan::backend`).
    pub backend: String,
}

/// Lifecycle of a session. Transitions happen only in `request_stop`
/// (`Recording -> Stopping -> Cancelled`) and `apply` (`-> Completed` on
/// `Finished`, `-> Failed` on `Error`); the last three states are terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Audio is captured and streamed; partials arrive.
    Recording,
    /// A stop was requested; the coordinator flushes and waits for
    /// `Finished`.
    Stopping,
    /// `Finished` arrived while stopping; the injection text (if any) can
    /// be taken once.
    Completed,
    /// A second stop, or a cancel during the flush, discarded the session;
    /// later events are ignored and nothing is injected.
    Cancelled,
    /// The backend reported an error; later events are ignored and nothing
    /// is injected.
    Failed,
}

/// What the coordinator should do after `request_stop`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAction {
    /// First stop: flush the audio and send `finish`.
    Finish,
    /// Stop while already stopping: abort the flush, never inject.
    Cancel,
    /// The session is already over; nothing to do.
    Ignore,
}

/// Result of applying one backend event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionUpdate {
    /// The live transcript changed (new partial or confirmed segment).
    Transcript(TranscriptSnapshot),
    /// The single final snapshot (`is_final == true`); the session is now
    /// `Completed`.
    Final(TranscriptSnapshot),
    /// The backend failed; the session is now `Failed`.
    Error(String),
    /// Nothing changed: `Ready`, any event after cancel or failure, or a
    /// `Finished` that arrived outside `Stopping`.
    Ignored,
}

/// The state machine itself: one per `run_session` call.
pub struct RecognitionSession {
    backend: String,
    state: SessionState,
    confirmed_segments: Vec<String>,
    partial_text: String,
    /// Set only by `Finished` while `Stopping`, cleared by cancel and
    /// error, handed out once by `take_injection_text`.
    injection_text: Option<String>,
}

impl RecognitionSession {
    /// Fresh session in `Recording` with an empty transcript.
    pub fn new(backend: impl Into<String>) -> Self {
        Self {
            backend: backend.into(),
            state: SessionState::Recording,
            confirmed_segments: Vec::new(),
            partial_text: String::new(),
            injection_text: None,
        }
    }

    /// Current lifecycle state.
    pub fn state(&self) -> SessionState {
        self.state
    }

    /// Register a stop request. `Recording -> Stopping` returns `Finish`
    /// (flush and send `finish`); a stop while already `Stopping` returns
    /// `Cancel`, moves to `Cancelled` and drops any injection text; in a
    /// terminal state it returns `Ignore`.
    pub fn request_stop(&mut self) -> SessionAction {
        match self.state {
            SessionState::Recording => {
                self.state = SessionState::Stopping;
                SessionAction::Finish
            }
            SessionState::Stopping => {
                self.state = SessionState::Cancelled;
                self.injection_text = None;
                SessionAction::Cancel
            }
            SessionState::Completed | SessionState::Cancelled | SessionState::Failed => {
                SessionAction::Ignore
            }
        }
    }

    /// Fold one backend event into the transcript. After `Cancelled` or
    /// `Failed` every event is ignored. `Partial` replaces the partial text
    /// (text plus stash); `Completed` appends a confirmed segment unless it
    /// repeats the previous one, and clears the partial; `Finished` is
    /// accepted only while `Stopping`: it completes the session, prefers
    /// the backend's full transcript over the joined segments, and keeps a
    /// non-empty result as the injection text; `Error` fails the session
    /// and clears the injection text; `Ready` changes nothing.
    pub fn apply(&mut self, event: crate::realtime::RealtimeEvent) -> SessionUpdate {
        if matches!(self.state, SessionState::Cancelled | SessionState::Failed) {
            return SessionUpdate::Ignored;
        }
        match event {
            crate::realtime::RealtimeEvent::Partial { text, stash } => {
                self.partial_text = format!("{text}{stash}");
                SessionUpdate::Transcript(self.snapshot(false))
            }
            crate::realtime::RealtimeEvent::Completed { transcript } => {
                // Backends may repeat the last confirmed segment (for
                // instance in the final flush); keep one copy.
                if !transcript.is_empty() && self.confirmed_segments.last() != Some(&transcript) {
                    self.confirmed_segments.push(transcript);
                }
                self.partial_text.clear();
                SessionUpdate::Transcript(self.snapshot(false))
            }
            crate::realtime::RealtimeEvent::Finished { transcript } => {
                // Only a `Finished` answering our `finish` is authoritative;
                // one arriving while still recording is dropped.
                if self.state != SessionState::Stopping {
                    return SessionUpdate::Ignored;
                }
                // Some backends send an empty final transcript; the
                // confirmed segments are then the best text we have.
                let authoritative_text = if transcript.is_empty() {
                    self.confirmed_segments.join("")
                } else {
                    transcript
                };
                self.state = SessionState::Completed;
                self.partial_text.clear();
                self.injection_text =
                    (!authoritative_text.is_empty()).then(|| authoritative_text.clone());
                SessionUpdate::Final(TranscriptSnapshot {
                    confirmed_segments: self.confirmed_segments.clone(),
                    partial_text: String::new(),
                    authoritative_text,
                    is_final: true,
                    backend: self.backend.clone(),
                })
            }
            crate::realtime::RealtimeEvent::Error(message) => {
                self.state = SessionState::Failed;
                self.injection_text = None;
                SessionUpdate::Error(message)
            }
            crate::realtime::RealtimeEvent::Ready => SessionUpdate::Ignored,
        }
    }

    /// Hand out the authoritative text for injection exactly once. `None`
    /// before completion, after cancel or error, when nothing was
    /// recognized, and on every later call.
    pub fn take_injection_text(&mut self) -> Option<String> {
        self.injection_text.take()
    }

    /// Snapshot of the live transcript: segments plus the current partial.
    fn snapshot(&self, is_final: bool) -> TranscriptSnapshot {
        TranscriptSnapshot {
            confirmed_segments: self.confirmed_segments.clone(),
            partial_text: self.partial_text.clone(),
            authoritative_text: self.confirmed_segments.join("") + &self.partial_text,
            is_final,
            backend: self.backend.clone(),
        }
    }
}
