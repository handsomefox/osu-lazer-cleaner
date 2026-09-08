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
/// A run from a shell detaches and leaves that shell's window alone. A double-click from
/// Explorer got a console of its own, which is hidden first so it shows for about a frame
/// rather than until the window appears.
#[cfg(windows)]
pub(crate) fn release() {
    use windows_sys::Win32::System::Console::{
        FreeConsole, GetConsoleProcessList, GetConsoleWindow,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};

    // Hiding a console window this process does not own would hide the terminal the user
    // launched it from. `GetConsoleProcessList` reports how many processes share the console;
    // one means Explorer allocated it for us alone.
    let mut owners = [0_u32; 2];

    // SAFETY: the buffer is ours and its length is passed correctly. A failure returns zero,
    // which is not one, so the window is left alone.
    let alone = unsafe { GetConsoleProcessList(owners.as_mut_ptr(), 2) } == 1;

    if alone {
        // SAFETY: `GetConsoleWindow` borrows nothing, and `ShowWindow` ignores the null handle
        // it returns when there is no console.
        let window = unsafe { GetConsoleWindow() };
        // SAFETY: the handle came from `GetConsoleWindow` and is not stored.
        unsafe { ShowWindow(window, SW_HIDE) };
    }

    // Detaching never touches another process's window, and destroys a console once its last
    // process leaves.
    //
    // SAFETY: borrows nothing, and is safe to call with no console attached.
    unsafe { FreeConsole() };
}

#[cfg(not(windows))]
pub(crate) fn release() {}
