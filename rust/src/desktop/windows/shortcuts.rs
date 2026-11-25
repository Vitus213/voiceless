//! RegisterHotKey owns each combination. The low-level hook supplies the release
//! transitions that WM_HOTKEY lacks. A replacement listener can share an existing
//! reservation until the daemon commits the new configuration.

use std::collections::BTreeMap;
use std::sync::{mpsc, Arc, Mutex, Weak};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetMessageW, PeekMessageW, PostThreadMessageW, MSG, PM_NOREMOVE, WM_APP, WM_QUIT,
};

use super::{failure, keys, lock, Chord};

type Combination = (u16, u8);
type Reply = mpsc::SyncSender<Result<(), String>>;
enum Request {
    Acquire(Combination, Reply),
    Release(Vec<Combination>, mpsc::SyncSender<()>),
}
struct Owner {
    id: u32,
    requests: mpsc::Sender<Request>,
}
static OWNER: Mutex<Weak<Owner>> = Mutex::new(Weak::new());

impl Owner {
    fn shared() -> Result<Arc<Self>, String> {
        let mut slot = lock(&OWNER);
        if let Some(owner) = slot.upgrade() {
            return Ok(owner);
        }
        let (requests, queue) = mpsc::channel();
        let (started, ready) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("voiceless-shortcuts".into())
            .spawn(move || unsafe {
                let mut msg = MSG::default();
                PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
                if started.send(GetCurrentThreadId()).is_err() {
                    return;
                }
                let mut bindings = BTreeMap::<Combination, (i32, usize)>::new();
                while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                    for request in queue.try_iter() {
                        match request {
                            Request::Acquire(combo, reply) => {
                                let result = if let Some((_, count)) = bindings.get_mut(&combo) {
                                    *count += 1;
                                    Ok(())
                                } else {
                                    // At most two generations of the six actions are live.
                                    let id = (1..0xc000)
                                        .find(|id| bindings.values().all(|(used, _)| used != id))
                                        .unwrap();
                                    if RegisterHotKey(
                                        std::ptr::null_mut(),
                                        id,
                                        modifiers(combo.1) | MOD_NOREPEAT,
                                        combo.0.into(),
                                    ) == 0
                                    {
                                        Err(failure("Windows could not reserve this shortcut"))
                                    } else {
                                        bindings.insert(combo, (id, 1));
                                        Ok(())
                                    }
                                };
                                let _ = reply.send(result);
                            }
                            Request::Release(combinations, released) => {
                                for combo in combinations {
                                    if let Some((id, count)) = bindings.get_mut(&combo) {
                                        *count -= 1;
                                        if *count == 0 {
                                            UnregisterHotKey(std::ptr::null_mut(), *id);
                                            bindings.remove(&combo);
                                        }
                                    }
                                }
                                let _ = released.send(());
                            }
                        }
                    }
                }
                // Thread-owned reservations expire when this thread exits.
            })
            .map_err(|error| error.to_string())?;
        let id = ready.recv().map_err(|error| error.to_string())?;
        let owner = Arc::new(Self { id, requests });
        *slot = Arc::downgrade(&owner);
        Ok(owner)
    }

    fn send(&self, request: Request) -> Result<(), String> {
        self.requests
            .send(request)
            .map_err(|error| error.to_string())?;
        if unsafe { PostThreadMessageW(self.id, WM_APP, 0, 0) } == 0 {
            return Err(failure("wake shortcut registration thread"));
        }
        Ok(())
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        unsafe {
            PostThreadMessageW(self.id, WM_QUIT, 0, 0);
        }
    }
}

fn modifiers(mask: u8) -> u32 {
    [
        (keys::CTRL, MOD_CONTROL),
        (keys::ALT, MOD_ALT),
        (keys::SHIFT, MOD_SHIFT),
        (keys::SUPER, MOD_WIN),
    ]
    .into_iter()
    .filter(|(bit, _)| mask & bit != 0)
    .fold(0, |flags, (_, flag)| flags | flag)
}

pub(super) struct Reservations {
    owner: Arc<Owner>,
    combinations: Vec<Combination>,
}
impl Drop for Reservations {
    fn drop(&mut self) {
        let (reply, released) = mpsc::sync_channel(1);
        if self
            .owner
            .send(Request::Release(
                std::mem::take(&mut self.combinations),
                reply,
            ))
            .is_ok()
        {
            let _ = released.recv();
        }
    }
}

pub(super) fn reserve(chords: &[Chord]) -> Result<Reservations, String> {
    let mut reservations = Reservations {
        owner: Owner::shared()?,
        combinations: Vec::with_capacity(chords.len() + 1),
    };
    for chord in chords {
        let legacy = [0, keys::CTRL];
        for &mask in chord
            .modifiers
            .as_ref()
            .map_or(&legacy[..], std::slice::from_ref)
        {
            let combo = (chord.key, mask);
            let (reply, answer) = mpsc::sync_channel(1);
            reservations.owner.send(Request::Acquire(combo, reply))?;
            answer
                .recv()
                .map_err(|error| error.to_string())?
                .map_err(|error| {
                    format!(
                        "hotkey.{}: {error}; choose another combination",
                        chord.action.field()
                    )
                })?;
            reservations.combinations.push(combo);
        }
    }
    Ok(reservations)
}
