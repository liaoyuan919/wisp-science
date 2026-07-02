//! Keep subprocesses from opening a console on Windows GUI builds.

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

pub fn hide_console(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
}

pub fn hide_console_async(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
}
