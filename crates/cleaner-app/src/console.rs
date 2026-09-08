//! Keeping the console out of the way of the window.
//!
//! One executable serves both interfaces, and Windows makes each of them want a different
//! subsystem. Linking for the window subsystem means a command-line run has no standard
//! handles, and worse, the shell does not wait for it: the prompt comes back while progress is
//! still being written over it.
//!
//! So the executable is linked for the console subsystem, where the command line behaves like
//! any other command-line tool, and the window releases the console it was given.

/// Releases the console this process was given, before the window opens.
///
/// A run from a shell keeps that shell's console, which stays where it was. A double-click
/// from Explorer got a console of its own, which closes here after showing for about a frame.
#[cfg(windows)]
pub(crate) fn release() {
    use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleWindow};
    use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};

    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "hiding the console window and freeing it are one operation"
    )]
    // SAFETY: both calls take no pointers we own and are safe to make with no console, where
    // `GetConsoleWindow` returns null and `ShowWindow` ignores it.
    unsafe {
        ShowWindow(GetConsoleWindow(), SW_HIDE);
        FreeConsole();
    }
}

#[cfg(not(windows))]
pub(crate) fn release() {}
