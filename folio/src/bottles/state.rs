//! Where bottles are: their states, as folio sees them, and what a command knows of them.

use super::*;

/// Where a bottle is, as folio sees it: its machine's state, once it's made; else how far its
/// making got; or why its state can't be told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Created,
    Running,
    Stopped,
    /// Its making hasn't finished.
    Creating,
    /// Deleting it left parts behind.
    Broken,
    /// There's no such bottle, or its machine is gone.
    Missing,
    /// Its runner can't be reached.
    Unreachable,
    /// Its runner is detached: not asked.
    Detached,
    /// Its thread's paused: it has a snapshot, and no bottle (threads::states).
    Paused,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Created => "created",
            State::Running => "running",
            State::Stopped => "stopped",
            State::Creating => "creating",
            State::Broken => "broken",
            State::Missing => "missing",
            State::Unreachable => "unreachable",
            State::Detached => "detached",
            State::Paused => "paused",
        }
    }

    /// Whether it's on a runner that's off: unreachable, or detached.
    pub fn runner_off(self) -> bool {
        matches!(self, State::Unreachable | State::Detached)
    }
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<listing::State> for State {
    fn from(state: listing::State) -> State {
        match state {
            listing::State::Created => State::Created,
            listing::State::Running => State::Running,
            listing::State::Stopped => State::Stopped,
        }
    }
}

/// How far a bottle's making got.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Made {
    /// Its setup hasn't finished.
    Creating,
    Ready,
    /// Deleting it left parts behind.
    Broken,
}

impl std::fmt::Display for Made {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Made::Creating => "creating",
            Made::Ready => "ready",
            Made::Broken => "broken",
        })
    }
}

/// A bottle's state, and its record if it has one.
#[derive(Clone, Debug)]
pub struct Status {
    pub name: String,
    pub state: State,
    pub bottle: Option<Bottle>,
}

/// What a command knows of bottles, so that what it does in several steps asks each runner once:
/// each bottle's status as it last asked (engine's status), and which it has found running or
/// started. Kept as the command changes them: a bottle made or deleted is forgotten, one started
/// (or run in, which starts it) is running. Made by each command, and given to what it asks.
pub struct Known {
    statuses: RefCell<BTreeMap<String, Status>>,
    running: RefCell<BTreeSet<String>>,
    /// folio serve, as this command has seen it.
    pub serve: daemon::Serve,
}

impl Default for Known {
    fn default() -> Known {
        Known {
            statuses: RefCell::default(),
            running: RefCell::default(),
            serve: daemon::Serve::new(bindings),
        }
    }
}

impl Known {
    /// What's known of `name`, if it was asked.
    pub fn status(&self, name: &str) -> Option<Status> {
        self.statuses.borrow().get(name).cloned()
    }

    pub fn learn(&self, status: Status) {
        if status.state == State::Running {
            self.running.borrow_mut().insert(status.name.clone());
        }
        self.statuses
            .borrow_mut()
            .insert(status.name.clone(), status);
    }

    /// What's known of `name` is out of date.
    pub fn forget(&self, name: &str) {
        self.statuses.borrow_mut().remove(name);
        self.running.borrow_mut().remove(name);
    }

    /// `name` is running now.
    pub fn started(&self, name: &str) {
        if let Some(status) = self.statuses.borrow_mut().get_mut(name) {
            status.state = State::Running;
        }
        self.running.borrow_mut().insert(name.to_string());
    }

    pub(super) fn running(&self, name: &str) -> bool {
        self.running.borrow().contains(name)
    }
}

/// Every bottle with its current state, or just those `names`: its machine's, unless setup never
/// finished, or its runner can't be reached or is detached. Only the runners these bottles are on
/// are asked, each once, all at once: a runner that's off costs what it takes to say so
/// (runners::LIST_TIMEOUT), however many there are.
pub fn list_all(names: Option<&[String]>) -> Result<Vec<(Bottle, State)>, String> {
    let mut chosen: Vec<Bottle> = load()?
        .into_values()
        .filter(|b| names.is_none_or(|n| n.contains(&b.name)))
        .collect();
    chosen.sort_by(|a, b| a.created.total_cmp(&b.created));
    let asked: BTreeSet<String> = chosen
        .iter()
        .filter(|b| b.status == Made::Ready)
        .map(|b| b.runner.clone())
        .collect();
    let remote = if asked.iter().any(|r| r != runners::LOCAL) {
        runners::registered()?
    } else {
        BTreeMap::new()
    };
    let detached: BTreeSet<String> = asked
        .iter()
        .filter(|r| *r != runners::LOCAL && remote.get(*r).copied().unwrap_or(false))
        .cloned()
        .collect();
    let to_ask: Vec<String> = asked.difference(&detached).cloned().collect();
    // Each runner asked, and its machines' states; None if it couldn't be reached.
    type Found = (
        String,
        Result<Option<BTreeMap<String, listing::State>>, String>,
    );
    let found: Vec<Found> = std::thread::scope(|scope| {
        let handles: Vec<_> = to_ask
            .iter()
            .map(|name| {
                let name = name.clone();
                scope.spawn(move || {
                    // The runner's machines' states; None if it can't be reached (this machine
                    // always can).
                    let states = runners::get(&name).and_then(|r| r.states());
                    let result = match states {
                        Ok(states) => Ok(Some(states)),
                        Err(e) if name == runners::LOCAL => Err(e),
                        Err(_) => Ok(None),
                    };
                    (name, result)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut states: BTreeMap<String, Option<BTreeMap<String, listing::State>>> = BTreeMap::new();
    for (name, result) in found {
        states.insert(name, result?);
    }
    Ok(chosen
        .into_iter()
        .map(|bottle| {
            let state = match bottle.status {
                Made::Creating => State::Creating,
                Made::Broken => State::Broken,
                Made::Ready if detached.contains(&bottle.runner) => State::Detached,
                Made::Ready => match &states[&bottle.runner] {
                    None => State::Unreachable,
                    Some(machines) => machines
                        .get(bottle.machine())
                        .map_or(State::Missing, |&s| s.into()),
                },
            };
            (bottle, state)
        })
        .collect())
}

/// Every bottle's state and record, or just those `names`. A name with no bottle is Missing, so a
/// caller can tell a bottle that's gone from a command that failed.
pub fn status(names: &[String]) -> Result<Vec<Status>, String> {
    let listed = list_all(if names.is_empty() { None } else { Some(names) })?;
    let order: Vec<String> = if names.is_empty() {
        listed.iter().map(|(b, _)| b.name.clone()).collect()
    } else {
        names.to_vec()
    };
    let mut states: BTreeMap<String, (Bottle, State)> = listed
        .into_iter()
        .map(|(b, s)| (b.name.clone(), (b, s)))
        .collect();
    Ok(order
        .into_iter()
        .map(|name| match states.remove(&name) {
            Some((bottle, state)) => Status {
                name,
                state,
                bottle: Some(bottle),
            },
            None => Status {
                name,
                state: State::Missing,
                bottle: None,
            },
        })
        .collect())
}
