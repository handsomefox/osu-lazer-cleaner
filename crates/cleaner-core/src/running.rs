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
    LAZER_PROCESSES.iter().any(|name| process_running(name))
}

/// Reports whether a process with this executable name is running, case-insensitively.
#[cfg(windows)]
#[expect(
    clippy::multiple_unsafe_ops_per_block,
    reason = "one Toolhelp enumeration owning a single snapshot handle"
)]
fn process_running(exe_name: &str) -> bool {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    // SAFETY: standard Toolhelp enumeration. Only process names are read, and the snapshot
    // handle is closed before returning.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return false;
        }

        let mut entry: PROCESSENTRY32W = zeroed();
        entry.dwSize = u32::try_from(size_of::<PROCESSENTRY32W>()).unwrap_or_default();

        let mut found = false;
        if Process32FirstW(snapshot, &raw mut entry) != 0 {
            loop {
                if wide_eq_ignore_case(&entry.szExeFile, exe_name) {
                    found = true;
                    break;
                }
                if Process32NextW(snapshot, &raw mut entry) == 0 {
                    break;
                }
            }
        }

        CloseHandle(snapshot);
        found
    }
}

/// Compares a NUL-terminated UTF-16 process name against an expected name.
#[cfg(windows)]
fn wide_eq_ignore_case(wide: &[u16], name: &str) -> bool {
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..end]).eq_ignore_ascii_case(name)
}

/// Reports whether a process with this name appears in `/proc`.
#[cfg(target_os = "linux")]
fn process_running(exe_name: &str) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };

    entries.flatten().any(|entry| {
        // Only numbered directories are processes; the rest of `/proc` is kernel state.
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(|c: char| c.is_ascii_digit())
        {
            return false;
        }
        std::fs::read_to_string(entry.path().join("comm"))
            .is_ok_and(|comm| comm.trim().eq_ignore_ascii_case(exe_name))
    })
}

#[cfg(not(any(windows, target_os = "linux")))]
fn process_running(_exe_name: &str) -> bool {
    false
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

    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_is_found_by_its_own_name() {
        let comm = std::fs::read_to_string("/proc/self/comm").unwrap();
        assert!(
            process_running(comm.trim()),
            "enumeration missed this process, named {:?}",
            comm.trim()
        );
        assert!(!process_running("a-process-that-does-not-exist"));
    }

    #[cfg(windows)]
    #[test]
    fn names_compare_without_regard_to_case() {
        let wide: Vec<u16> = "OSU!.EXE\0".encode_utf16().collect();
        assert!(wide_eq_ignore_case(&wide, "osu!.exe"));
        assert!(!wide_eq_ignore_case(&wide, "osu!"));
    }
}
