//! Puts each command in its own process group, so killing it kills the whole
//! tree it started.
//!
//! `cargo bench` runs the compiled benchmark as a grandchild. Killing only the
//! direct child would leave that benchmark running — burning CPU and
//! corrupting every later measurement on the machine.

use std::process::{Child, Command};

/// Makes the child the leader of a new process group.
pub fn configure(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: setpgid is async-signal-safe and is exactly what pre_exec is
    // for; it touches no memory shared with the parent.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Kills the child and everything it started, by signalling the group.
pub fn kill_tree(child: &mut Child) {
    let pid = child.id() as i32;
    // Negative pid addresses the whole group. SIGKILL rather than SIGTERM: by
    // the time this runs the command has already overrun its timeout.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    let _ = child.kill();
}
