use std::sync::atomic::{AtomicU8, Ordering};

const RUNNING: u8 = 0;
const QUITTING: u8 = 1;
const RESTART_REQUESTED: u8 = 2;

static SHUTDOWN_STATE: AtomicU8 = AtomicU8::new(RUNNING);

pub fn mark_quitting() {
    SHUTDOWN_STATE.store(QUITTING, Ordering::SeqCst);
}

pub fn request_restart_for_second_launch_if_quitting() -> bool {
    SHUTDOWN_STATE
        .compare_exchange(
            QUITTING,
            RESTART_REQUESTED,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_ok()
}

#[cfg(test)]
fn reset_for_test() {
    SHUTDOWN_STATE.store(RUNNING, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::{mark_quitting, request_restart_for_second_launch_if_quitting, reset_for_test};

    #[test]
    fn second_launch_during_quit_requests_one_restart() {
        reset_for_test();
        assert!(!request_restart_for_second_launch_if_quitting());

        mark_quitting();
        assert!(request_restart_for_second_launch_if_quitting());
        assert!(!request_restart_for_second_launch_if_quitting());
    }
}
