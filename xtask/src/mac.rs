//! The Mac's distribution, built on a Mac: bottle-cli, signed with the hypervisor entitlement.

use std::env;
use std::path::Path;
use std::process::Command;

use super::{Profile, agents, beside_cli, cargo, copy, root, run, same_time, step, target};

use super::Result;

/// The Mac's distribution, built on a Mac. mke2fs and the kernel, while cargo builds.
pub(super) fn mac_dist(profile: Profile, out: &Path) -> Result<()> {
    std::thread::scope(|scope| {
        let beside = scope.spawn(|| {
            beside_cli(
                profile,
                "aarch64-apple-darwin",
                env::consts::ARCH,
                None,
                out,
            )
        });
        let built = mac_cli(profile, out);
        let beside = beside
            .join()
            .map_err(|_| "building mke2fs or the kernel panicked")?;
        built.and(beside)
    })
}

/// The Mac's bottle-cli, the agent built in, signed.
pub(super) fn mac_cli(profile: Profile, out: &Path) -> Result<()> {
    let agent = agents(profile, &[env::consts::ARCH])?.remove(0);
    step("bottle-cli, the agent built in");
    let mut args = vec!["build"];
    args.extend(profile.packages());
    args.extend(profile.cargo_args());
    cargo(&args, &[("BOTTLE_EMBED_AGENT".into(), agent.into())])?;
    let built = target().join(profile.dir()).join("bottle-cli");
    let cli = out.join("bottle-cli");
    if copy(&built, &cli)? {
        step("signing bottle-cli");
        let entitlements = root().join("xtask/hypervisor.entitlements");
        run(Command::new("codesign")
            .args(["--force", "-s", "-", "--entitlements"])
            .arg(&entitlements)
            .arg(&cli))?;
        // Signing changed it: its time says again what it's a copy of (see copy).
        same_time(&built, &cli)?;
    }
    if profile == Profile::Release {
        let built = target().join(profile.dir()).join("folio");
        let folio = out.join("folio");
        if copy(&built, &folio)? {
            // Signed afresh, as stripping it may have left its signature behind.
            step("signing folio");
            run(Command::new("codesign")
                .args(["--force", "-s", "-"])
                .arg(&folio))?;
            same_time(&built, &folio)?;
        }
    }
    Ok(())
}
