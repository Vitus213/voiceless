use super::{DaemonCommand, Gateway, HotkeyMachine, KeyAction, SessionRunner, State};
use crate::config::HotkeyConfig;
use crate::hotkey::{Action, KeyEvent, Transition};

pub(super) struct Gestures {
    legacy: HotkeyMachine,
    mode: String,
    held: Option<(Action, u64)>,
}

impl Gestures {
    pub(super) fn new(config: &HotkeyConfig) -> Self {
        Self {
            legacy: HotkeyMachine::new(
                std::time::Duration::from_millis(config.hold_threshold_ms),
                config.mode.clone(),
                config.ctrl_mode.clone(),
            ),
            mode: config.mode.clone(),
            held: None,
        }
    }

    pub(super) fn command<R: SessionRunner>(
        &mut self,
        event: KeyEvent,
        gateway: &Gateway<R>,
    ) -> Option<DaemonCommand> {
        if let Transition::Press { ctrl } = event.transition {
            return match event.action {
                Action::Legacy => match self.legacy.on_press(gateway.state(), ctrl, event.at) {
                    KeyAction::Start(mode) => {
                        Some(self.start(event.action, gateway.last_session_id + 1, mode))
                    }
                    KeyAction::Stop => {
                        self.held = None;
                        Some(DaemonCommand::Stop { reply: None })
                    }
                    KeyAction::Nothing => None,
                },
                Action::Hold if gateway.state() == State::Idle => {
                    Some(self.start(event.action, gateway.last_session_id + 1, self.mode.clone()))
                }
                Action::Hold => None,
                Action::Toggle => Some(DaemonCommand::Toggle {
                    mode: self.mode.clone(),
                    reply: None,
                }),
                Action::Raw => Some(DaemonCommand::Toggle {
                    mode: crate::modes::RAW_MODE_ID.to_owned(),
                    reply: None,
                }),
                Action::Cancel => Some(DaemonCommand::Cancel { reply: None }),
                Action::Repaste => Some(DaemonCommand::Repaste { reply: None }),
            };
        }

        let owns_recording = self
            .held
            .take_if(|(action, _)| *action == event.action)
            .is_some_and(|(_, session_id)| {
                gateway.session_id() == Some(session_id) && gateway.state() == State::Recording
            });
        if event.action == Action::Legacy {
            let state = if owns_recording && event.transition == Transition::Release {
                State::Recording
            } else {
                State::Idle
            };
            if self.legacy.on_release(state, event.at) == KeyAction::Stop {
                return Some(DaemonCommand::Stop { reply: None });
            }
        }
        if !owns_recording {
            return None;
        }
        if event.transition == Transition::Disconnected {
            Some(DaemonCommand::Cancel { reply: None })
        } else if event.action == Action::Hold {
            Some(DaemonCommand::Stop { reply: None })
        } else {
            None
        }
    }

    fn start(&mut self, action: Action, session_id: u64, mode: String) -> DaemonCommand {
        self.held = Some((action, session_id));
        DaemonCommand::Start { mode, reply: None }
    }
}
