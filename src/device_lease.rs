use std::sync::{Condvar, Mutex, MutexGuard};

// one accelerator device, one batch at a time: an encode waits for it, the player gives way
pub struct DeviceLease {
    state: Mutex<LeaseState>,
    changed: Condvar,
}

#[derive(Default)]
struct LeaseState {
    encode_holds: bool,
    encodes_waiting: u32,
    playback_holds: bool,
}

pub static DEVICE_LEASE: DeviceLease = DeviceLease {
    state: Mutex::new(LeaseState {
        encode_holds: false,
        encodes_waiting: 0,
        playback_holds: false,
    }),
    changed: Condvar::new(),
};

pub struct EncodeLease<'a>(&'a DeviceLease);
pub struct PlaybackLease<'a>(&'a DeviceLease);

impl DeviceLease {
    fn lock(&self) -> MutexGuard<'_, LeaseState> {
        self.state.lock().expect("device lease lock")
    }

    // blocks until the player has ended its batch and no other encode holds the device
    pub fn acquire_for_encode(&self) -> EncodeLease<'_> {
        let mut state = self.lock();
        state.encodes_waiting += 1;
        while state.encode_holds || state.playback_holds {
            state = self.changed.wait(state).expect("device lease lock");
        }
        state.encodes_waiting -= 1;
        state.encode_holds = true;
        EncodeLease(self)
    }

    // the player takes the device only while no encode holds or waits for it
    pub fn try_acquire_for_playback(&self) -> Option<PlaybackLease<'_>> {
        let mut state = self.lock();
        if state.encode_holds || state.encodes_waiting > 0 || state.playback_holds {
            return None;
        }
        state.playback_holds = true;
        Some(PlaybackLease(self))
    }

    pub fn encode_wants_the_device(&self) -> bool {
        let state = self.lock();
        state.encode_holds || state.encodes_waiting > 0
    }
}

impl Drop for EncodeLease<'_> {
    fn drop(&mut self) {
        self.0.lock().encode_holds = false;
        self.0.changed.notify_all();
    }
}

impl Drop for PlaybackLease<'_> {
    fn drop(&mut self) {
        self.0.lock().playback_holds = false;
        self.0.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease() -> DeviceLease {
        DeviceLease {
            state: Mutex::new(LeaseState::default()),
            changed: Condvar::new(),
        }
    }

    #[test]
    fn the_player_gives_way_to_an_encode() {
        let lease = lease();
        let playback = lease.try_acquire_for_playback().expect("free device");
        assert!(!lease.encode_wants_the_device());
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let encode = lease.acquire_for_encode();
                assert!(
                    lease.try_acquire_for_playback().is_none(),
                    "the player must not take the device under an encode"
                );
                drop(encode);
            });
            // the encode is queued behind the player, which ends its batch when it sees that
            while !lease.encode_wants_the_device() {
                std::thread::yield_now();
            }
            assert!(
                lease.try_acquire_for_playback().is_none(),
                "a waiting encode already blocks a new playback lease"
            );
            drop(playback);
        });
        assert!(lease.try_acquire_for_playback().is_some());
    }

    #[test]
    fn two_encodes_take_turns() {
        let lease = lease();
        let first = lease.acquire_for_encode();
        std::thread::scope(|scope| {
            let second = scope.spawn(|| lease.acquire_for_encode());
            while lease.lock().encodes_waiting == 0 {
                std::thread::yield_now();
            }
            drop(first);
            drop(second.join().expect("second encode"));
        });
        assert!(lease.try_acquire_for_playback().is_some());
    }
}
