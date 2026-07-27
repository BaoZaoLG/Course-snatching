// App-level alerts: in-app toast queue + optional Windows beep.
use std::sync::Mutex;

#[derive(Debug, Clone)]
pub struct AppAlert {
    pub title: String,
    pub body: String,
    pub success: bool,
}

static PENDING: Mutex<Vec<AppAlert>> = Mutex::new(Vec::new());

/// Queue an in-app toast only. Does not play sound.
pub fn push_toast(title: impl Into<String>, body: impl Into<String>, success: bool) {
    if let Ok(mut queue) = PENDING.lock() {
        queue.push(AppAlert {
            title: title.into(),
            body: body.into(),
            success,
        });
        if queue.len() > 20 {
            let drain = queue.len() - 20;
            queue.drain(0..drain);
        }
    }
}

/// Play the system alert sound only. Does not queue a toast.
pub fn play_sound(success: bool) {
    #[cfg(windows)]
    beep(success);
    #[cfg(not(windows))]
    let _ = success;
}

/// Dispatch toast and/or sound according to independent switches.
pub fn dispatch_alert(
    title: impl Into<String>,
    body: impl Into<String>,
    success: bool,
    notify_enabled: bool,
    sound_enabled: bool,
) {
    let title = title.into();
    let body = body.into();
    if notify_enabled {
        push_toast(title, body, success);
    } else if sound_enabled {
        // Avoid allocating toast strings into the queue when only sound is wanted.
        play_sound(success);
        return;
    }
    if sound_enabled {
        play_sound(success);
    }
}

/// Backward-compatible helper: both toast and sound.
#[allow(dead_code)]
pub fn push_alert(title: impl Into<String>, body: impl Into<String>, success: bool) {
    dispatch_alert(title, body, success, true, true);
}

pub fn take_alerts() -> Vec<AppAlert> {
    PENDING
        .lock()
        .map(|mut queue| std::mem::take(&mut *queue))
        .unwrap_or_default()
}

#[cfg(windows)]
fn beep(success: bool) {
    // Prefer native kernel32 Beep over spawning PowerShell for every alert.
    let freq = if success { 880u32 } else { 420u32 };
    let dur = if success { 180u32 } else { 280u32 };
    // Beep blocks for `dur` ms; keep UI/worker threads free.
    std::thread::spawn(move || unsafe {
        windows_sys::Win32::System::Diagnostics::Debug::Beep(freq, dur);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // Both tests exercise the same process-global alert queue. Rust runs unit
    // tests concurrently by default, so keep the clear/dispatch/take sequence
    // atomic with respect to the other queue test.
    static TEST_QUEUE_LOCK: Mutex<()> = Mutex::new(());

    fn lock_test_queue() -> std::sync::MutexGuard<'static, ()> {
        TEST_QUEUE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn clear_queue() {
        let _ = take_alerts();
    }

    #[test]
    fn dispatch_alert_four_switch_combinations() {
        let _queue_guard = lock_test_queue();
        clear_queue();

        // Both off: nothing queued.
        dispatch_alert("t", "b", true, false, false);
        assert!(take_alerts().is_empty());

        // Notify only: toast, no dependency on sound.
        dispatch_alert("n", "only", true, true, false);
        let alerts = take_alerts();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].title, "n");
        assert_eq!(alerts[0].body, "only");

        // Sound only: no toast queued.
        dispatch_alert("s", "only", false, false, true);
        assert!(take_alerts().is_empty());

        // Both on: toast queued (sound is side-effect).
        dispatch_alert("both", "yes", true, true, true);
        let alerts = take_alerts();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].title, "both");
        assert!(alerts[0].success);
    }

    #[test]
    fn push_toast_does_not_require_sound_path() {
        let _queue_guard = lock_test_queue();
        clear_queue();
        push_toast("hello", "world", false);
        let alerts = take_alerts();
        assert_eq!(alerts.len(), 1);
        assert!(!alerts[0].success);
    }
}
