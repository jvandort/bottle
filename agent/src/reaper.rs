//! Waiting for children, all in one place.
//!
//! The agent is PID 1 in its VM (or a subreaper outside one), so every orphan in the VM becomes
//! its child and must be reaped, or it stays a zombie. One thread reaps them all, whichever
//! exits; were each command's thread to wait for its own child as well, the reaper could
//! take that child's status first. So commands are spawned through [`Reaper::spawn`], which hands
//! the reaper a channel for the child's status before the reaper can see it exit.
//!
//! The reaper only peeks at an exited child (`waitid` with `WNOWAIT`) until it holds the lock that
//! spawning holds, and reaps it after: so a child is never reaped before [`Reaper::spawn`] has
//! opened its pidfd, which then signals that process or nothing, never another with its pid.

use std::collections::HashMap;
use std::io;
use std::os::fd::OwnedFd;
use std::process::{Child, Command};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use bottle_agent_proto::Exit;

#[derive(Default)]
struct State {
    /// The children spawned through [`Reaper::spawn`] that haven't exited, and where their status goes.
    waiting: HashMap<u32, Sender<Exit>>,
}

#[derive(Clone)]
pub struct Reaper {
    inner: Arc<(Mutex<State>, Condvar)>,
}

impl Reaper {
    /// Starts the thread that reaps every child of this process.
    pub fn start() -> Self {
        let reaper = Reaper {
            inner: Arc::default(),
        };
        let inner = Arc::clone(&reaper.inner);
        thread::Builder::new()
            .name("reaper".into())
            .spawn(move || reap(&inner))
            .expect("start the reaper");
        reaper
    }

    /// Spawns `command`, returning the child, a pidfd for it, and a receiver for its status when
    /// it exits. Never wait for the child itself: the reaper does.
    pub fn spawn(&self, command: &mut Command) -> io::Result<(Child, OwnedFd, Receiver<Exit>)> {
        let (lock, wake) = &*self.inner;
        // Held while spawning, so the child is in `waiting`, and its pidfd open, before the reaper
        // can reap it.
        let mut state = lock.lock().unwrap();
        let child = command.spawn()?;
        // The child can't have been reaped yet: the reaper reaps only with the lock.
        let pid = rustix::process::Pid::from_child(&child);
        let pidfd = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())?;
        let (tx, rx) = channel();
        state.waiting.insert(child.id(), tx);
        wake.notify_all();
        Ok((child, pidfd, rx))
    }
}

fn reap(inner: &(Mutex<State>, Condvar)) {
    use rustix::io::Errno;
    use rustix::process::{WaitId, WaitIdOptions, WaitOptions, wait, waitid};
    let (lock, wake) = inner;
    loop {
        // Waits for a child to exit, leaving it to be reaped below, with the lock: so a child
        // spawned is in `waiting`, its pidfd open, before it can be.
        match waitid(WaitId::All, WaitIdOptions::EXITED | WaitIdOptions::NOWAIT) {
            Ok(_) => {
                let mut state = lock.lock().unwrap();
                if let Ok(Some((pid, status))) = wait(WaitOptions::NOHANG) {
                    let exit = match status.terminating_signal() {
                        Some(signal) => Exit::Signal(signal),
                        None => Exit::Code(status.exit_status().unwrap_or(0)),
                    };
                    // An orphan's status nobody's waiting for: it's reaped, and that's all.
                    if let Some(tx) = state.waiting.remove(&(pid.as_raw_nonzero().get() as u32)) {
                        let _ = tx.send(exit);
                    }
                }
            }
            Err(Errno::INTR) => {}
            // No children: sleep until one's spawned. Orphans only come from children, but a
            // timeout costs nothing and covers whatever this misses.
            Err(Errno::CHILD) => {
                let state = lock.lock().unwrap();
                let _ = wake.wait_timeout(state, Duration::from_secs(1)).unwrap();
            }
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
}
