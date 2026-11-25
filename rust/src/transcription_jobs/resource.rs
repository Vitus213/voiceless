//! The shared CPU/GPU resource, and how live dictation and background
//! work divide it.
//!
//! A background job borrows the resource for one bounded unit at a time:
//! at most one second of media through the segmenter, plus the decode of
//! the segments that second closed. It never holds it between units, so a
//! live dictation session that needs the same decoder waits for at most
//! one unit.
//!
//! [`PriorityResource`] is the gate the daemon puts between its sessions
//! and the queue. A session holds it ([`PriorityResource::hold`]) for as
//! long as it runs. While any hold lives no unit that uses the decoder
//! starts; the one that was in flight when the hold was taken is what
//! [`PriorityHold::wait_for_background`] waits for, and the units that
//! waited resume once the hold is gone. A job that waits reports
//! [`Activity::WaitingForResource`](super::job::Activity::WaitingForResource),
//! so the page can say when background work is held and when it resumes.
//!
//! Jobs have two lanes. A local job decodes on the device the daemon's
//! engine runs on, so its units take the decoder lane, which holds govern.
//! A cloud job sends its segments to a service and only segments audio on
//! the CPU on the way: its units take a lane of their own, one HTTP
//! request at a time, which no hold blocks and which blocks no session.
//! A cloud request that waits on the network is therefore never a hold on
//! the decoder.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use super::job::Transport;

/// How long a waiting unit sleeps before it looks again, so that `abort`
/// is noticed even when nothing wakes it.
const POLL: Duration = Duration::from_millis(50);

/// A resource background jobs borrow for one bounded unit at a time.
///
/// The queue hands its workers the resource as `dyn SharedResource`; the
/// guards that release it again are made with the methods of that type.
pub trait SharedResource: Send + Sync {
    /// Block until the resource is free for one bounded unit. Returns
    /// `false`, holding nothing, when `abort` asks to give up waiting.
    fn acquire(&self, abort: &dyn Fn() -> bool) -> bool;

    /// Give back a unit [`SharedResource::acquire`] granted.
    fn release(&self);

    /// [`SharedResource::acquire`] for a job sent over `transport`. A
    /// resource that does not tell transports apart has the one unit.
    fn acquire_for(&self, _transport: Transport, abort: &dyn Fn() -> bool) -> bool {
        self.acquire(abort)
    }

    /// Give back a unit [`SharedResource::acquire_for`] granted.
    fn release_for(&self, _transport: Transport) {
        self.release();
    }
}

impl dyn SharedResource + '_ {
    /// [`SharedResource::acquire`] with the guard that calls
    /// [`SharedResource::release`]. The guard is only made once the unit
    /// is granted: it releases on drop.
    pub fn unit(&self, abort: &dyn Fn() -> bool) -> Option<ResourceUnit<'_>> {
        self.acquire(abort).then(|| ResourceUnit {
            resource: self,
            transport: None,
        })
    }

    /// [`SharedResource::acquire_for`] with the guard that calls
    /// [`SharedResource::release_for`]. The guard is only made once the
    /// unit is granted: it releases on drop.
    pub fn unit_for(
        &self,
        transport: Transport,
        abort: &dyn Fn() -> bool,
    ) -> Option<ResourceUnit<'_>> {
        self.acquire_for(transport, abort).then(|| ResourceUnit {
            resource: self,
            transport: Some(transport),
        })
    }
}

/// One borrowed unit. Dropping it releases the resource.
pub struct ResourceUnit<'a> {
    resource: &'a dyn SharedResource,
    /// The transport the unit was granted for; `None` for a unit of
    /// [`SharedResource::acquire`].
    transport: Option<Transport>,
}

impl Drop for ResourceUnit<'_> {
    fn drop(&mut self) {
        match self.transport {
            Some(transport) => self.resource.release_for(transport),
            None => self.resource.release(),
        }
    }
}

#[derive(Default)]
struct Gate {
    /// Live holds.
    foreground: usize,
    /// A local unit holds the decoder.
    decoder_busy: bool,
    /// A cloud unit is waiting on its request.
    cloud_busy: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lane {
    Decoder,
    Cloud,
}

fn lane(transport: Transport) -> Lane {
    match transport {
        Transport::Local => Lane::Decoder,
        Transport::Cloud => Lane::Cloud,
    }
}

/// Live dictation has priority over background work on the decoder.
///
/// Decoder units never overlap, and none starts while a hold lives.
/// Holds do not exclude each other: the daemon's gateway already admits
/// one session at a time, and that is where the hold belongs.
#[derive(Clone)]
pub struct PriorityResource {
    state: Arc<(Mutex<Gate>, Condvar)>,
}

impl Default for PriorityResource {
    fn default() -> Self {
        Self::new()
    }
}

impl PriorityResource {
    pub fn new() -> Self {
        Self {
            state: Arc::new((Mutex::new(Gate::default()), Condvar::new())),
        }
    }

    /// Claim the decoder for live dictation until the hold is dropped.
    pub fn hold(&self) -> PriorityHold {
        self.state.0.lock().foreground += 1;
        PriorityHold {
            state: Arc::clone(&self.state),
        }
    }

    fn acquire_lane(&self, lane: Lane, abort: &dyn Fn() -> bool) -> bool {
        let mut gate = self.state.0.lock();
        loop {
            if abort() {
                return false;
            }
            let free = match lane {
                Lane::Decoder => gate.foreground == 0 && !gate.decoder_busy,
                Lane::Cloud => !gate.cloud_busy,
            };
            if free {
                match lane {
                    Lane::Decoder => gate.decoder_busy = true,
                    Lane::Cloud => gate.cloud_busy = true,
                }
                return true;
            }
            self.state.1.wait_for(&mut gate, POLL);
        }
    }

    fn release_lane(&self, lane: Lane) {
        let mut gate = self.state.0.lock();
        match lane {
            Lane::Decoder => gate.decoder_busy = false,
            Lane::Cloud => gate.cloud_busy = false,
        }
        self.state.1.notify_all();
    }
}

impl SharedResource for PriorityResource {
    fn acquire(&self, abort: &dyn Fn() -> bool) -> bool {
        self.acquire_lane(Lane::Decoder, abort)
    }

    fn release(&self) {
        self.release_lane(Lane::Decoder)
    }

    fn acquire_for(&self, transport: Transport, abort: &dyn Fn() -> bool) -> bool {
        self.acquire_lane(lane(transport), abort)
    }

    fn release_for(&self, transport: Transport) {
        self.release_lane(lane(transport))
    }
}

/// The live claim of [`PriorityResource::hold`].
pub struct PriorityHold {
    state: Arc<(Mutex<Gate>, Condvar)>,
}

impl PriorityHold {
    /// Block until no background unit holds the decoder.
    ///
    /// Called with this hold in place, which keeps new decoder units from
    /// starting, the wait ends when the unit in flight ends: one segment
    /// decode at most. `false` when `timeout` passes first, which means a
    /// decoder did not come back from its unit.
    pub fn wait_for_background(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut gate = self.state.0.lock();
        while gate.decoder_busy {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            self.state.1.wait_for(&mut gate, remaining);
        }
        true
    }
}

impl Drop for PriorityHold {
    fn drop(&mut self) {
        self.state.0.lock().foreground -= 1;
        self.state.1.notify_all();
    }
}
