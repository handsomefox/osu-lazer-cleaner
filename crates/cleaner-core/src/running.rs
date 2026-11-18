//! Telling whether osu!lazer has the library open.
//!
//! Cleaning, restoring, and compacting refuse to start while osu!lazer runs, with
//! [`SnapshotError::LazerRunning`](crate::SnapshotError::LazerRunning). The Realm write lock
//! alone does not protect them. osu!lazer's `RealmFileStore.Add` writes a blob into `files/`
//! before the transaction that adds its usage commits, so a clean that runs in between sees a
//! blob nothing uses and removes it. Compaction copies `client.realm` without a lock, and
//! `RealmFileStore.Cleanup` runs on the game's own startup.
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
/// enumerate processes. The process check is the main guard against cleaning under a running
/// game. Unreferenced blobs also have to sit unmodified for
/// [`UNREFERENCED_MIN_AGE`](crate::storage::UNREFERENCED_MIN_AGE) before a clean takes them,
/// which covers a game the check missed or one started after it.
#[must_use]
pub fn lazer_is_running() -> bool {
    #[cfg(test)]
    {
        // Tests decide the answer, so that they pass whether or not osu!lazer is open on the
        // machine running them.
        tests::PRETEND_RUNNING.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        detect()
    }
}

/// Looks for an osu!lazer process.
fn detect() -> bool {
    let running = app_process::Running::snapshot();
    LAZER_PROCESSES.iter().any(|name| running.contains(name))
}

/// Fails with [`SnapshotError::LazerRunning`](crate::SnapshotError::LazerRunning) while
/// osu!lazer runs.
pub(crate) fn ensure_lazer_closed() -> Result<(), crate::SnapshotError> {
    if lazer_is_running() {
        tracing::warn!("refused: osu!lazer is running");
        return Err(crate::SnapshotError::LazerRunning);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    thread_local! {
        /// What [`lazer_is_running`] answers on this test's thread.
        pub(crate) static PRETEND_RUNNING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// Makes [`lazer_is_running`] report osu!lazer as running for the rest of this test.
    pub(crate) fn pretend_lazer_runs() {
        PRETEND_RUNNING.with(|running| running.set(true));
    }

    #[test]
    fn the_check_answers_without_panicking() {
        // osu!lazer is not running under the test harness, and a machine where it is would
        // still only flip the answer, never fail.
        let _: bool = detect();
    }

    #[test]
    fn a_running_game_refuses_the_operation() {
        assert!(ensure_lazer_closed().is_ok());
        pretend_lazer_runs();
        assert!(matches!(
            ensure_lazer_closed(),
            Err(crate::SnapshotError::LazerRunning)
        ));
    }
}
