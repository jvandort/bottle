//! A running VM's upkeep, by exec, from the process that runs it, as long as it runs: its clock set
//! from this machine's, and the memory its guest has freed given back.
//!
//! - **The clock**: a VM's starts from its RTC, which counts whole seconds, so it starts behind
//!   (and a certificate this machine made a moment ago, bottled's for a bottle, isn't valid yet
//!   there), and drifts. Set with `date`, when the agent first answers, then at every round.
//! - **Memory**: the guest gives back free memory only in whole 2 MiB blocks (free page
//!   reporting, at libkrun's Replace granularity), and keeps every file it read cached. So the
//!   kernel is asked to compact in the background (vm.compaction_proactiveness), and every round
//!   the page cache is dropped and memory compacted: what's in the cache of the stage disks this
//!   machine caches too, so reading it again is cheap.
//!
//! All through `sh` and `date`, as root: an image without them (a stage being made on nothing)
//! gets none of it.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bottle_agent_client::{Exit, Start, run};

/// How often the guest's cache is dropped and its clock set again.
const ROUND: Duration = Duration::from_secs(5 * 60);

/// 0 to 100: how hard the guest's kernel compacts memory in the background, unasked (20 by
/// default). Compaction it can't make progress with, it defers.
const COMPACTION_PROACTIVENESS: u32 = 100;

/// Keeps the VM whose agent is at `socket`, on a thread of its own, until this process ends.
pub fn spawn(socket: PathBuf) {
    thread::spawn(move || {
        if bottle_disk::build::wait_for_agent(&socket, || true).is_err() {
            return;
        }
        set_clock(&socket);
        shell(
            &socket,
            &format!("echo {COMPACTION_PROACTIVENESS} > /proc/sys/vm/compaction_proactiveness"),
        );
        loop {
            thread::sleep(ROUND);
            shell(
                &socket,
                "sync; echo 1 > /proc/sys/vm/drop_caches; echo 1 > /proc/sys/vm/compact_memory",
            );
            set_clock(&socket);
        }
    });
}

/// Sets the guest's clock to this machine's: GNU date takes it to the nanosecond, BusyBox's to
/// the second.
fn set_clock(socket: &Path) {
    for precise in [true, false] {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let at = if precise {
            format!("@{}.{:09}", now.as_secs(), now.subsec_nanos())
        } else {
            format!("@{}", now.as_secs())
        };
        if command(socket, vec!["date".into(), "-u".into(), "-s".into(), at]) {
            return;
        }
    }
}

fn shell(socket: &Path, script: &str) {
    command(socket, vec!["sh".into(), "-c".into(), script.into()]);
}

/// Runs `argv` in the guest, as root, with nothing in or out: whether it exited 0.
fn command(socket: &Path, argv: Vec<String>) -> bool {
    let (_, controls) = channel();
    let start = Start {
        argv,
        ..Start::default()
    };
    matches!(
        run(
            socket,
            start,
            io::empty(),
            &mut io::sink(),
            &mut io::sink(),
            controls
        ),
        Ok(Exit::Code(0))
    )
}
