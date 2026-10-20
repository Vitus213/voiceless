#![allow(dead_code)]

//! Shared fakes for coordinator lifecycle tests. Each fake records its calls
//! in a shared log so tests can assert ordering at the system boundaries.

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::mpsc;
use voiceless::coordinator::{
    AudioCapture, BackendTransport, ControlCommand, EventSink, HistoryEntry, HistoryRecorder,
    InjectionResult, OutputEvent, TextInjector, TextProcessor,
};
use voiceless::realtime::RealtimeEvent;

pub type Log = Arc<Mutex<Vec<String>>>;

pub fn new_log() -> Log {
    Arc::new(Mutex::new(Vec::new()))
}

pub fn entries(log: &Log) -> Vec<String> {
    log.lock().clone()
}

pub fn ok_fake_injection() -> InjectionResult {
    InjectionResult {
        ok: true,
        method: "fake".to_owned(),
        message: String::new(),
    }
}

#[derive(Default)]
pub struct RecordingSink {
    pub events: Vec<OutputEvent>,
}

impl EventSink for RecordingSink {
    fn emit(&mut self, event: OutputEvent) -> io::Result<()> {
        self.events.push(event);
        Ok(())
    }
}

pub struct FakeCapture {
    pub log: Log,
    pub chunks: VecDeque<Vec<u8>>,
    pub tail: Option<Vec<u8>>,
    pub eof_on_empty: bool,
    pub start_error: Option<String>,
    pub stop_error: Option<String>,
    pub chunk_interval: Duration,
}

impl FakeCapture {
    pub fn new(log: Log, chunks: Vec<Vec<u8>>, tail: Option<Vec<u8>>) -> Self {
        Self {
            log,
            chunks: chunks.into(),
            tail,
            eof_on_empty: false,
            start_error: None,
            stop_error: None,
            chunk_interval: Duration::ZERO,
        }
    }
}

impl AudioCapture for FakeCapture {
    async fn start(&mut self) -> io::Result<()> {
        self.log.lock().push("capture.start".to_owned());
        if let Some(message) = self.start_error.take() {
            return Err(io::Error::other(message));
        }
        Ok(())
    }

    async fn next_chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        if self.chunk_interval > Duration::ZERO {
            tokio::time::sleep(self.chunk_interval).await;
        }
        if let Some(chunk) = self.chunks.pop_front() {
            self.log
                .lock()
                .push(format!("capture.chunk:{}", chunk.len()));
            return Ok(Some(chunk));
        }
        if self.eof_on_empty {
            return Ok(None);
        }
        std::future::pending::<()>().await;
        unreachable!("pending future resolved")
    }

    async fn stop_capture(&mut self) -> io::Result<Option<Vec<u8>>> {
        self.log.lock().push("capture.stop".to_owned());
        if let Some(message) = self.stop_error.take() {
            return Err(io::Error::other(message));
        }
        Ok(self.tail.take())
    }

    fn abort(&mut self) {
        self.log.lock().push("capture.abort".to_owned());
    }
}

struct TransportState {
    before_finish: VecDeque<io::Result<RealtimeEvent>>,
    after_finish: VecDeque<io::Result<RealtimeEvent>>,
    finished: bool,
    appends: usize,
    triggers: Vec<(usize, ControlCommand)>,
    fail_after_appends: Option<usize>,
}

pub struct ScriptedTransport {
    pub log: Log,
    state: Mutex<TransportState>,
    pub control_tx: Option<mpsc::Sender<ControlCommand>>,
    pub append_delay: Duration,
    pub finish_error: Mutex<Option<String>>,
    pub hang_on_finish: bool,
}

impl ScriptedTransport {
    pub fn new(log: Log) -> Self {
        Self {
            log,
            state: Mutex::new(TransportState {
                before_finish: VecDeque::new(),
                after_finish: VecDeque::new(),
                finished: false,
                appends: 0,
                triggers: Vec::new(),
                fail_after_appends: None,
            }),
            control_tx: None,
            append_delay: Duration::ZERO,
            finish_error: Mutex::new(None),
            hang_on_finish: false,
        }
    }

    pub fn before_finish(&self, event: io::Result<RealtimeEvent>) {
        self.state.lock().before_finish.push_back(event);
    }

    pub fn after_finish(&self, event: io::Result<RealtimeEvent>) {
        self.state.lock().after_finish.push_back(event);
    }

    pub fn trigger(&self, at: usize, command: ControlCommand) {
        self.state.lock().triggers.push((at, command));
    }
}

impl BackendTransport for ScriptedTransport {
    async fn send_audio(&self, pcm: &[u8]) -> io::Result<()> {
        if self.append_delay > Duration::ZERO {
            tokio::time::sleep(self.append_delay).await;
        }
        let mut fired = Vec::new();
        {
            let mut state = self.state.lock();
            if state.fail_after_appends == Some(state.appends) {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "transport send failed",
                ));
            }
            self.log
                .lock()
                .push(format!("transport.append:{}", pcm.len()));
            state.appends += 1;
            let appends = state.appends;
            state.triggers.retain(|(at, command)| {
                if *at == appends {
                    fired.push(*command);
                    false
                } else {
                    true
                }
            });
        }
        if let Some(tx) = &self.control_tx {
            for command in fired {
                let _ = tx.send(command).await;
            }
        }
        Ok(())
    }

    async fn next_event(&self) -> io::Result<RealtimeEvent> {
        loop {
            {
                let mut state = self.state.lock();
                if let Some(event) = state.before_finish.pop_front() {
                    return event;
                }
                if state.finished {
                    if let Some(event) = state.after_finish.pop_front() {
                        return event;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    async fn finish(&mut self) -> io::Result<()> {
        self.log.lock().push("transport.finish".to_owned());
        if self.hang_on_finish {
            std::future::pending::<()>().await;
            unreachable!("pending future resolved")
        }
        if let Some(message) = self.finish_error.lock().take() {
            return Err(io::Error::other(message));
        }
        self.state.lock().finished = true;
        Ok(())
    }

    async fn cancel(&mut self) -> io::Result<()> {
        self.log.lock().push("transport.cancel".to_owned());
        Ok(())
    }
}

pub struct RecordingInjector {
    pub log: Log,
    pub result_ok: bool,
}

impl RecordingInjector {
    pub fn new(log: Log) -> Self {
        Self {
            log,
            result_ok: true,
        }
    }
}

impl TextInjector for RecordingInjector {
    async fn inject(&mut self, text: &str) -> InjectionResult {
        self.log.lock().push(format!("injector.inject:{text}"));
        InjectionResult {
            ok: self.result_ok,
            method: "fake".to_owned(),
            message: String::new(),
        }
    }
}

#[derive(Clone)]
pub struct CountingProcessor {
    output: Result<String, String>,
    state: Arc<Mutex<(usize, Option<String>)>>,
}

impl CountingProcessor {
    pub fn ok(output: String) -> Self {
        Self {
            output: Ok(output),
            state: Arc::new(Mutex::new((0, None))),
        }
    }

    pub fn failing(error: String) -> Self {
        Self {
            output: Err(error),
            state: Arc::new(Mutex::new((0, None))),
        }
    }

    pub fn noop() -> Self {
        Self::ok(String::new())
    }

    pub fn calls(&self) -> usize {
        self.state.lock().0
    }

    pub fn last_input(&self) -> Option<String> {
        self.state.lock().1.clone()
    }
}

impl TextProcessor for CountingProcessor {
    async fn process(&mut self, _mode_id: &str, text: &str) -> Result<String, String> {
        {
            let mut state = self.state.lock();
            state.0 += 1;
            state.1 = Some(text.to_owned());
        }
        self.output.clone()
    }
}

#[derive(Clone, Default)]
pub struct RecordingHistory {
    entries: Arc<Mutex<Vec<HistoryEntry>>>,
}

impl RecordingHistory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn records(&self) -> Vec<HistoryEntry> {
        self.entries.lock().clone()
    }
}

impl HistoryRecorder for RecordingHistory {
    fn record(&mut self, entry: HistoryEntry) {
        self.entries.lock().push(entry);
    }
}
