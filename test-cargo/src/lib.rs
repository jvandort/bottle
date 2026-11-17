//! For tests that build another package of the workspace (a program cargo doesn't hand them): the
//! cargo that's building them, as whoever owns the build. Run as root by a sudo runner (for the
//! tests that need root), a build of root's would leave files in the target directory that its
//! owner's next build can't write, so it's run as the user sudo was run by.

use std::path::Path;
use std::process::Command;

/// This build's cargo, to be given its arguments: under sudo, as sudo's user, with cargo's own
/// directory, where rustc is, first on PATH (sudo sets its own).
pub fn cargo() -> Command {
    let cargo = env!("CARGO");
    match std::env::var("SUDO_USER") {
        Ok(user) if rustix::process::geteuid().is_root() => {
            let bin = Path::new(cargo).parent().unwrap().display().to_string();
            let path = std::env::var("PATH").unwrap_or_default();
            let mut command = Command::new("sudo");
            command.args([
                "-E",
                "-u",
                &user,
                "env",
                &format!("PATH={bin}:{path}"),
                cargo,
            ]);
            command
        }
        _ => Command::new(cargo),
    }
}
