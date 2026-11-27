//! The gateway consumes the same chord transitions from every desktop.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;

use crate::desktop::native as platform;
pub(crate) use crate::desktop::native_keys::Chord as Binding;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Legacy,
    Hold,
    Toggle,
    Cancel,
    Raw,
    Repaste,
}

const ACTIONS: [Action; 6] = [
    Action::Legacy,
    Action::Hold,
    Action::Toggle,
    Action::Cancel,
    Action::Raw,
    Action::Repaste,
];

impl Action {
    pub(crate) fn field(self) -> &'static str {
        match self {
            Self::Legacy => "key",
            Self::Hold => "hold_key",
            Self::Toggle => "toggle_key",
            Self::Cancel => "cancel_key",
            Self::Raw => "raw_key",
            Self::Repaste => "repaste_key",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transition {
    Press { ctrl: bool },
    Release,
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub(crate) action: Action,
    pub(crate) transition: Transition,
    pub(crate) at: Instant,
}

pub(crate) fn configured_keys(config: &crate::config::HotkeyConfig) -> [Option<&str>; 6] {
    [
        config.key.as_deref(),
        config.hold_key.as_deref(),
        config.toggle_key.as_deref(),
        config.cancel_key.as_deref(),
        config.raw_key.as_deref(),
        config.repaste_key.as_deref(),
    ]
}

pub fn parse_key(name: &str) -> Result<u16, String> {
    platform::key_code(name).ok_or_else(|| format!("{name} is unavailable on this system"))
}

pub(crate) fn bindings(config: &crate::config::HotkeyConfig) -> Result<Vec<Binding>, String> {
    let mut bindings: Vec<Binding> = Vec::new();
    for (action, name) in ACTIONS.into_iter().zip(configured_keys(config)) {
        let Some(name) = name else { continue };
        let (key, modifiers) = parse_binding(name, action)
            .map_err(|error| format!("hotkey.{}: {error}", action.field()))?;
        if let Some(other) = bindings.iter().find(|other| {
            other.key == key
                && (other.modifiers == modifiers
                    || (other.modifiers.is_none() && matches!(modifiers, Some(0 | 1))))
        }) {
            return Err(format!(
                "hotkey.{} conflicts with hotkey.{} on {name}",
                action.field(),
                other.action.field()
            ));
        }
        bindings.push(Binding {
            action,
            key,
            modifiers,
        });
    }
    Ok(bindings)
}

fn parse_binding(name: &str, action: Action) -> Result<(u16, Option<u8>), String> {
    if action == Action::Legacy && !crate::config::is_chord(name) {
        return parse_key(name).map(|key| (key, None));
    }
    let mut parts = name.rsplit('+');
    let key = parse_key(parts.next().unwrap_or_default())?;
    let mut modifiers = 0;
    for part in parts {
        let bit = match part {
            "Ctrl" => 1,
            "Alt" => 2,
            "Shift" => 4,
            "Super" => 8,
            _ => return Err(format!("unknown modifier {part:?}")),
        };
        if modifiers & bit != 0 {
            return Err(format!("duplicate modifier {part}"));
        }
        modifiers |= bit;
    }
    Ok((key, Some(modifiers)))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct KeyboardSupport {
    pub(crate) total: usize,
    counts: [usize; 6],
}

impl KeyboardSupport {
    pub(crate) fn for_action(self, action: Action) -> usize {
        self.counts[action as usize]
    }
}

pub(crate) struct Listener {
    native: Option<platform::KeyListener>,
    support: KeyboardSupport,
    error: Option<String>,
    events: mpsc::Sender<KeyEvent>,
}

impl Listener {
    pub fn open(bindings: Arc<[Binding]>, events: mpsc::Sender<KeyEvent>) -> Self {
        let mut support = KeyboardSupport::default();
        let (native, error) = match platform::start_keys(bindings.clone(), events.clone()) {
            Ok(native) => {
                support.total = 1;
                for binding in bindings.iter() {
                    support.counts[binding.action as usize] = 1;
                }
                (Some(native), None)
            }
            Err(error) => (None, Some(error)),
        };
        Self {
            native,
            support,
            events,
            error,
        }
    }

    pub fn support(&self) -> KeyboardSupport {
        self.support
    }

    pub(crate) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn start(
        self,
        mut on_count: impl FnMut(KeyboardSupport) + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        on_count(self.support);
        let Self { native, events, .. } = self;
        tokio::spawn(async move {
            events.closed().await;
            drop(native);
        })
    }
}
