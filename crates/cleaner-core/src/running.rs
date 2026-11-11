//! Telling whether osu!lazer has the library open.
//!
//! Every operation that writes to the library wants the game closed. A clean holds the Realm
//! write lock while it works, compaction is declined outright while another handle is open, and
//! `RealmFileStore.Cleanup` runs on the game's own startup. Asking the user to close osu!lazer
//! is not the same as knowing whether they did, so this checks.
//!
//! Detection is by process name, which is what osu!lazer's own single-instance check uses. It
//! never opens a handle to the game.

/// Process names osu!lazer runs under.
///
/// The Windows executable is `osu!.exe`. The Linux builds report `osu!`, which the
/// 15-character limit on `comm` leaves untouched.
const LAZER_PROCESSES: &[&str] = &["osu!.exe", "osu!"];

/// Reports whether osu!lazer is running.
///
/// Returns `false` on any platform where the check is not implemented, and on any failure to
/// enumerate processes. A missed detection only means the user sees the same advice they saw
/// before; the operations themselves are guarded by the database lock, not by this.
#[must_use]
pub fn lazer_is_running() -> bool {
    let running = app_process::Running::snapshot();
    LAZER_PROCESSES.iter().any(|name| running.contains(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_check_answers_without_panicking() {
        // osu!lazer is not running under the test harness, and a machine where it is would
        // still only flip the answer, never fail.
        let _: bool = lazer_is_running();
    }
}
