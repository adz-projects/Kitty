//! Small cross-cutting helpers.

use std::path::Path;
use std::process::Command;

/// Build a [`Command`] that does not flash a console window on Windows.
pub fn hidden_command(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}
