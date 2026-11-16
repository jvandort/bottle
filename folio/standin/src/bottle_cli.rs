//! bottle-cli, for tests that drive folio: $BOTTLE_CLI is this, linked as `bottle-cli`.
//!
//! Its machines, images and every call are in $FAKE_CLI_STATE, a JSON file. A machine is what it
//! was made with (its arguments, environment, mounts, labels and network policy), its state, and a
//! directory of its own beside the state file, which stands in for its disk: what runs in it runs
//! here, in that directory, with its home there ($HOME, and /home/genie), its repos there
//! (/workspace and /notes), and each mount's target its source here. Its repos' remotes at
//! bottle.host are empty repos of its own: it has no network.
//!
//! An exec is recorded, with what it was given on stdin, and answered as a machine that meets the
//! bottle contract would: the checks pass (or name what $FAKE_CLI_MISSING says is missing), the
//! hosts and directories a bottle's set up with are taken as given, a key script says a key, and
//! shpool is pretended, its sessions kept in the state (a session in a machine's "exited" is one
//! whose program has, which shpool lists all the same). Anything else runs, its paths in the
//! machine made its directory's. An image build records its recipe and policy, and the image is
//! then built, with the recipe's labels.
//!
//! $FAKE_CLI_FAIL makes steps fail (a comma-separated list), as bottle-cli would: create,
//! configure, start, delete, build, or an exec's contract, hosts, mkdir or workspace (create and
//! delete for one machine alone as create:NAME, delete:NAME); failures
//! look like bottle-cli's, `bottle-cli: ...` on stderr, exit 1. $FAKE_CLI_SLOW makes them take
//! $FAKE_CLI_SLOW_SECONDS (default 30), to be interrupted. An exec whose command has
//! $FAKE_CLI_GARBLE in it writes "not a tarball" instead; one with $FAKE_CLI_FAIL_EXEC in it exits
//! 3.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use serde_json::{Value, json};

/// Where a thread's machine has its repos, and its user's home.
const GUEST_REPOS: [&str; 2] = ["/workspace", "/notes"];
const GUEST_HOME: &str = "/home/genie";
/// Where a bottle's remotes are, in it.
const REMOTES: &str = "http://bottle.host/git/";
/// A pretended session's pid: above Linux's highest, so no process has it. One whose program has
/// exited has another, which kill -0 doesn't find either.
const NO_PID: u64 = 4194305;
const EXITED_PID: u64 = 4194306;

fn state_path() -> PathBuf {
    PathBuf::from(std::env::var_os("FAKE_CLI_STATE").expect("FAKE_CLI_STATE"))
}

/// The state, held for as long as this is: one call's changes are whole.
struct State {
    path: PathBuf,
    value: Value,
    _lock: fs::File,
}

impl State {
    fn open() -> State {
        let path = state_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let lock = fs::File::create(path.with_extension("lock")).unwrap();
        // SAFETY: flock on a file this owns.
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
        let mut value: Value = fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_else(|| json!({}));
        for (key, empty) in [
            ("machines", json!({})),
            ("images", json!([])),
            ("calls", json!([])),
            ("execs", json!([])),
            ("builds", json!([])),
        ] {
            if value.get(key).is_none() {
                value[key] = empty;
            }
        }
        State {
            path,
            value,
            _lock: lock,
        }
    }

    /// Written whole or not at all: a test reads it as it's written.
    fn save(&self) {
        let staging = self.path.with_file_name(format!(
            ".{}.{}",
            self.path.file_name().unwrap().to_string_lossy(),
            std::process::id()
        ));
        fs::write(&staging, serde_json::to_string_pretty(&self.value).unwrap()).unwrap();
        fs::rename(&staging, &self.path).unwrap();
    }

    fn machine(&mut self, name: &str) -> Option<&mut Value> {
        self.value["machines"].get_mut(name)
    }
}

/// bottle-cli's own failure.
fn fail(message: &str) -> ExitCode {
    eprintln!("bottle-cli: {message}");
    ExitCode::from(1)
}

fn listed(variable: &str, step: &str) -> bool {
    std::env::var(variable).is_ok_and(|v| v.split(',').any(|s| s == step))
}

fn failing(step: &str) -> bool {
    listed("FAKE_CLI_FAIL", step)
}

/// Whether `step` fails for machine `name`: as every machine's does, or as STEP:NAME says.
fn failing_for(step: &str, name: &str) -> bool {
    failing(step) || listed("FAKE_CLI_FAIL", &format!("{step}:{name}"))
}

/// A step $FAKE_CLI_SLOW names takes its time: long enough to be interrupted.
fn slow(step: &str) {
    if listed("FAKE_CLI_SLOW", step) {
        let seconds: f64 = std::env::var("FAKE_CLI_SLOW_SECONDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30.0);
        std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
    }
}

/// Each option in `takes` (one that takes a value), its values in order; the rest, positional.
fn options(args: &[String], takes: &[&str]) -> (Vec<(String, String)>, Vec<String>) {
    let (mut given, mut rest) = (Vec::new(), Vec::new());
    let mut i = 0;
    while i < args.len() {
        if takes.contains(&args[i].as_str()) && i + 1 < args.len() {
            given.push((args[i].clone(), args[i + 1].clone()));
            i += 2;
        } else {
            rest.push(args[i].clone());
            i += 1;
        }
    }
    (given, rest)
}

fn values<'a>(given: &'a [(String, String)], option: &str) -> Vec<&'a str> {
    given
        .iter()
        .filter(|(o, _)| o == option)
        .map(|(_, v)| v.as_str())
        .collect()
}

fn stdin_text() -> String {
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text).unwrap();
    text
}

/// Which of a bottle's setup steps an exec is, by what it runs.
fn step_of(argv: &[String]) -> &'static str {
    let script = match argv {
        [sh, c, script] if sh == "sh" && c == "-c" => script.as_str(),
        _ => "",
    };
    if script.starts_with("# bottle contract") {
        "contract"
    } else if script.contains("/etc/hosts") {
        "hosts"
    } else if script.starts_with("install -d -o") {
        "mkdir"
    } else if script.contains("safe.directory") || script.contains("BOTTLE.md") {
        "workspace"
    } else {
        ""
    }
}

pub fn main(args: Vec<String>) -> ExitCode {
    {
        let mut state = State::open();
        state.value["calls"]
            .as_array_mut()
            .unwrap()
            .push(json!(args));
        state.save();
    }
    let strs: Vec<&str> = args.iter().map(String::as_str).collect();
    match strs.as_slice() {
        ["info", "--json"] => {
            let memory: u64 = std::env::var("FAKE_CLI_MEMORY")
                .ok()
                .and_then(|m| m.parse().ok())
                .unwrap_or(16384);
            let home = state_path().parent().unwrap().join("bottle-home");
            println!("{}", json!({"memory_mib": memory, "cpus": 8, "home": home}));
            ExitCode::SUCCESS
        }
        ["images", "--json"] => {
            println!("{}", State::open().value["images"]);
            ExitCode::SUCCESS
        }
        ["image", "build", recipe, "--tag", tag, "--network", network] => {
            if failing("build") {
                return fail("image build failed");
            }
            let read = |path: &str| -> Value {
                serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
            };
            let (recipe, network) = (read(recipe), read(network));
            let mut state = State::open();
            let builds = state.value["builds"].as_array_mut().unwrap();
            builds.push(json!({"tag": tag, "recipe": recipe, "network": network}));
            let stage = format!("stage-{}", builds.len());
            let images = state.value["images"].as_array_mut().unwrap();
            images.retain(|i| i["tag"] != *tag);
            images.push(json!({"tag": tag, "stage": stage, "disks": [format!("{stage}.ext4")],
                               "labels": recipe.get("labels").cloned().unwrap_or_else(|| json!({}))}));
            state.save();
            println!("/stages/{stage}.ext4");
            ExitCode::SUCCESS
        }
        ["image", "rm", tag] => {
            let mut state = State::open();
            state.value["images"]
                .as_array_mut()
                .unwrap()
                .retain(|i| i["tag"] != *tag);
            state.save();
            ExitCode::SUCCESS
        }
        ["gc"] => ExitCode::SUCCESS,
        ["create", name, ..] => create(name, &args[2..]),
        ["configure", name, "--network", "-"] => {
            if failing("configure") {
                return fail("configure failed");
            }
            let network: Value = serde_json::from_str(&stdin_text()).unwrap();
            let mut state = State::open();
            let Some(machine) = state.machine(name) else {
                return fail(&format!("no machine {name}"));
            };
            machine["network"] = network;
            machine["configured"] = json!(machine["configured"].as_u64().unwrap_or(0) + 1);
            state.save();
            ExitCode::SUCCESS
        }
        ["start", name] => {
            slow("start");
            if failing("start") {
                return fail("start failed");
            }
            let mut state = State::open();
            let Some(machine) = state.machine(name) else {
                return fail(&format!("no machine {name}"));
            };
            machine["state"] = "running".into();
            machine["started"] = json!(machine["started"].as_u64().unwrap_or(0) + 1);
            state.save();
            ExitCode::SUCCESS
        }
        ["stop", name] => {
            let mut state = State::open();
            let Some(machine) = state.machine(name) else {
                return fail(&format!("no machine {name}"));
            };
            machine["state"] = "stopped".into();
            machine["sessions"] = json!({});
            state.save();
            ExitCode::SUCCESS
        }
        ["delete", name, "-f"] => {
            slow("delete");
            if failing_for("delete", name) {
                return fail("machine busy");
            }
            let mut state = State::open();
            state.value["machines"]
                .as_object_mut()
                .unwrap()
                .remove(*name);
            state.save();
            let _ = fs::remove_dir_all(directory(name));
            ExitCode::SUCCESS
        }
        ["status", name, "--json"] => {
            let mut state = State::open();
            let Some(machine) = state.machine(name) else {
                return fail(&format!("no machine {name}"));
            };
            println!("{}", json!({"name": name, "state": machine["state"]}));
            ExitCode::SUCCESS
        }
        ["ls", "--json"] => {
            let state = State::open();
            let rows: Vec<Value> = state.value["machines"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(n, m)| json!({"name": n, "state": m["state"]}))
                .collect();
            println!("{}", json!(rows));
            ExitCode::SUCCESS
        }
        ["exec", name, ..] => exec(name, &args[2..]),
        ["cp", src, dst] => copy(src, dst),
        _ => fail(&format!("the stand-in doesn't know {args:?}")),
    }
}

/// A machine's directory: its disk, as far as what runs in it goes.
fn directory(name: &str) -> PathBuf {
    state_path().parent().unwrap().join("machines").join(name)
}

fn create(name: &str, rest: &[String]) -> ExitCode {
    if failing_for("create", name) {
        return fail("create failed");
    }
    let (given, _) = options(
        rest,
        &[
            "--image",
            "--cpus",
            "--mem",
            "--storage",
            "--user",
            "--workdir",
            "--label",
            "-v",
            "-e",
            "--network",
        ],
    );
    let network: Value = if values(&given, "--network") == ["-"] {
        serde_json::from_str(&stdin_text()).unwrap()
    } else {
        Value::Null
    };
    let mut state = State::open();
    if state.machine(name).is_some() {
        return fail(&format!("machine {name} already exists"));
    }
    let single = |option: &str| {
        values(&given, option)
            .first()
            .map(|v| json!(v))
            .unwrap_or(Value::Null)
    };
    let pairs = |option: &str| -> Value {
        values(&given, option)
            .iter()
            .filter_map(|p| p.split_once('='))
            .map(|(k, v)| (k.to_string(), json!(v)))
            .collect::<serde_json::Map<_, _>>()
            .into()
    };
    state.value["machines"][name] = json!({
        "state": "created", "image": single("--image"), "cpus": single("--cpus"), "mem": single("--mem"),
        "storage": single("--storage"), "user": single("--user"), "workdir": single("--workdir"),
        "labels": pairs("--label"), "mounts": values(&given, "-v"), "env": pairs("-e"),
        "network": network, "started": 0, "sessions": {}, "exited": [],
    });
    state.save();
    // Its home, as an image gives it.
    let home = directory(name).join("home");
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::write(
        home.join(".claude/settings.json"),
        r#"{"from": "the image"}"#,
    )
    .unwrap();
    ExitCode::SUCCESS
}

/// How paths in a machine are this machine's: each mount's target its source, its repos and home
/// its directory's, its remotes at bottle.host empty repos of its own.
struct Paths {
    here: PathBuf,
    /// Longest first: a path in a mount is that mount's, not one it's in.
    mounts: Vec<(String, String)>,
}

impl Paths {
    fn of(name: &str, machine: &Value) -> Paths {
        let here = directory(name);
        let mut mounts: Vec<(String, String)> = machine["mounts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter_map(|v| {
                let v = v.strip_suffix(":ro").unwrap_or(v);
                v.rsplit_once(':')
                    .map(|(source, target)| (target.to_string(), source.to_string()))
            })
            .collect();
        for repo in GUEST_REPOS {
            mounts.push((repo.into(), format!("{}{repo}", here.display())));
        }
        mounts.push((GUEST_HOME.into(), here.join("home").display().to_string()));
        mounts.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
        Paths { here, mounts }
    }

    /// `arg`, its paths in the machine made this machine's: each at the start of a word, or after
    /// one of =:'"( , and ending there or at a / or the end of a word.
    fn here(&self, arg: &str) -> String {
        let bytes = arg.as_bytes();
        let mut out = String::new();
        let mut i = 0;
        'scan: while i < arg.len() {
            let starts = i == 0
                || !matches!(bytes[i - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'/' | b'.' | b'-');
            if starts {
                if arg[i..].starts_with(REMOTES) {
                    let end = arg[i..]
                        .find(|c: char| c.is_whitespace() || c == '\'' || c == '"')
                        .map_or(arg.len(), |n| i + n);
                    out.push_str(&self.remote(&arg[i + REMOTES.len()..end]));
                    i = end;
                    continue;
                }
                for (from, to) in &self.mounts {
                    if arg[i..].starts_with(from.as_str()) {
                        let next = bytes.get(i + from.len());
                        if next.is_none_or(|c| !matches!(c, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b'.')) {
                            out.push_str(to);
                            i += from.len();
                            continue 'scan;
                        }
                    }
                }
            }
            let c = arg[i..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
        }
        out
    }

    /// A remote at bottle.host: an empty repo of the machine's own.
    fn remote(&self, path: &str) -> String {
        let repo = self.here.join("remotes").join(format!("{path}.git"));
        if !repo.exists() {
            let _ = Command::new("git")
                .arg("init")
                .arg("-q")
                .arg("--bare")
                .arg(&repo)
                .status();
        }
        repo.display().to_string()
    }

    /// A path here, as the machine has it.
    fn there(&self, path: &str) -> String {
        for (from, to) in &self.mounts {
            if let Some(rest) = path.strip_prefix(to.as_str())
                && (rest.is_empty() || rest.starts_with('/'))
            {
                return format!("{from}{rest}");
            }
        }
        path.to_string()
    }
}

/// A shell's words, as `sh` reads them: '...' and "..." quoting, and \ escaping.
fn words(line: &str) -> Vec<String> {
    let (mut words, mut word, mut quoted) = (Vec::new(), String::new(), false);
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                quoted = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    word.push(c);
                }
            }
            '"' => {
                quoted = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => word.extend(chars.next()),
                        c => word.push(c),
                    }
                }
            }
            '\\' => word.extend(chars.next()),
            c if c.is_whitespace() => {
                if !word.is_empty() || quoted {
                    words.push(std::mem::take(&mut word));
                }
                quoted = false;
            }
            c => word.push(c),
        }
    }
    if !word.is_empty() || quoted {
        words.push(word);
    }
    words
}

fn exec(name: &str, rest: &[String]) -> ExitCode {
    let (given, positional) = options(rest, &["-u", "-w", "-e"]);
    let Some(dash) = positional.iter().position(|a| a == "--") else {
        return fail("exec needs -- and a command");
    };
    let tty = positional[..dash].iter().any(|a| a == "-t");
    let command: Vec<String> = positional[dash + 1..].to_vec();
    let machine = {
        let mut state = State::open();
        let Some(machine) = state.machine(name) else {
            return fail(&format!("no machine {name}"));
        };
        if machine["state"] != "running" {
            return fail(&format!("{name} isn't running"));
        }
        machine.clone()
    };
    let stdin = if tty {
        Vec::new()
    } else {
        let mut bytes = Vec::new();
        std::io::stdin().read_to_end(&mut bytes).unwrap();
        bytes
    };
    let step = step_of(&command);
    {
        let mut state = State::open();
        state.value["execs"].as_array_mut().unwrap().push(json!({
            "machine": name, "user": values(&given, "-u").first(), "workdir": values(&given, "-w").first(),
            "tty": tty, "argv": command, "stdin": String::from_utf8_lossy(&stdin), "step": step,
        }));
        state.save();
    }
    if !step.is_empty() && failing(step) {
        return fail(&format!("{step} failed"));
    }
    let joined = command.join(" ");
    let has = |variable: &str| {
        std::env::var(variable).is_ok_and(|v| !v.is_empty() && joined.contains(&v))
    };
    if has("FAKE_CLI_GARBLE") {
        print!("not a tarball");
        return ExitCode::SUCCESS;
    }
    if has("FAKE_CLI_FAIL_EXEC") {
        return ExitCode::from(3);
    }
    let paths = Paths::of(name, &machine);
    match step {
        "contract" => {
            if let Ok(missing) = std::env::var("FAKE_CLI_MISSING")
                && !missing.is_empty()
            {
                println!("{missing}");
            }
            return ExitCode::SUCCESS;
        }
        "hosts" => return ExitCode::SUCCESS,
        // As root, it makes them genie's: here, they're made.
        "mkdir" => {
            for word in words(&command[2])
                .iter()
                .skip(1)
                .filter(|w| w.starts_with('/'))
            {
                fs::create_dir_all(paths.here(word)).unwrap();
            }
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    if let Some(code) = pretended(name, &command) {
        return code;
    }
    let script = match command.as_slice() {
        [sh, c, script] if sh == "sh" && c == "-c" => script.as_str(),
        _ => "",
    };
    if script.contains("ssh_host_ed25519_key") {
        println!("ssh-ed25519 HOSTKEY-OF-{name}");
        return ExitCode::SUCCESS;
    }
    if script.contains("id_ed25519") {
        println!("ssh-ed25519 KEY-OF-{name}");
        return ExitCode::SUCCESS;
    }
    if script.contains("authorized_keys") || script.contains("known_hosts") {
        return ExitCode::SUCCESS;
    }
    run(&paths, &machine, &given, &command, tty, &stdin)
}

/// shpool and its sessions, as a bottle's are kept: pretended. None if `command` isn't one.
fn pretended(name: &str, command: &[String]) -> Option<ExitCode> {
    let c: Vec<&str> = command.iter().map(String::as_str).collect();
    let mut state = State::open();
    let machine = state.machine(name)?;
    let code = match c.as_slice() {
        ["shpool", "list", "--json"] => {
            let exited: Vec<&str> = machine["exited"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let sessions: Vec<Value> = machine["sessions"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(n, _)| json!({"name": n, "shell_pid": if exited.contains(&n.as_str()) { EXITED_PID } else { NO_PID }}))
                .collect();
            println!("{}", json!({"sessions": sessions}));
            ExitCode::SUCCESS
        }
        [
            "env",
            _,
            "shpool",
            "attach",
            "--background",
            "--dir",
            _,
            "--cmd",
            what,
            session,
        ] => {
            let program = words(what).get(2).cloned().unwrap_or_default();
            machine["sessions"][*session] = json!(program);
            ExitCode::SUCCESS
        }
        ["kill", "-0", pid] => {
            return Some(if *pid == NO_PID.to_string() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            });
        }
        ["shpool", "kill", session] => {
            machine["sessions"]
                .as_object_mut()
                .map(|s| s.remove(*session));
            if let Some(exited) = machine["exited"].as_array_mut() {
                exited.retain(|n| n != session);
            }
            ExitCode::SUCCESS
        }
        // Attached: the call log is the evidence.
        ["sh", "-c", _, "attach", _, _, _] => return Some(ExitCode::SUCCESS),
        ["kill", "-HUP", ..] => {
            machine["sessions"] = json!({});
            ExitCode::SUCCESS
        }
        _ => return None,
    };
    state.save();
    Some(code)
}

/// `command` run here, as in the machine: in its working directory, with its environment and home,
/// its paths the machine's directory's.
fn run(
    paths: &Paths,
    machine: &Value,
    given: &[(String, String)],
    command: &[String],
    tty: bool,
    stdin: &[u8],
) -> ExitCode {
    let home = paths.here.join("home");
    fs::create_dir_all(&home).unwrap();
    let argv: Vec<String> = command.iter().map(|a| paths.here(a)).collect();
    let workdir = values(given, "-w")
        .first()
        .map(|w| w.to_string())
        .or_else(|| machine["workdir"].as_str().map(String::from))
        .map(|w| PathBuf::from(paths.here(&w)))
        .filter(|w| w.is_dir())
        .unwrap_or_else(|| paths.here.clone());
    let mut child = Command::new(&argv[0]);
    child
        .args(&argv[1..])
        .current_dir(&workdir)
        .env("HOME", &home)
        // git's config, global to the machine: its home's, not what the test has given its own.
        .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"));
    for (key, value) in machine["env"].as_object().into_iter().flatten() {
        child.env(key, paths.here(value.as_str().unwrap_or_default()));
    }
    for pair in values(given, "-e") {
        if let Some((key, value)) = pair.split_once('=') {
            child.env(key, paths.here(value));
        }
    }
    if !tty {
        child.stdin(Stdio::piped());
    }
    let mut child = match child.spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("bottle-cli: {}: {e}", argv[0]);
            return ExitCode::from(127);
        }
    };
    if let Some(mut pipe) = child.stdin.take() {
        let stdin = stdin.to_vec();
        std::thread::spawn(move || {
            let _ = pipe.write_all(&stdin);
        });
    }
    let status = child.wait().unwrap();
    ExitCode::from(
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)) as u8,
    )
}

/// One side of a copy: in a machine (its directory's path for it, and its name), or here.
fn side(arg: &str) -> Result<(PathBuf, Option<(String, Paths)>), String> {
    if let Some((name, path)) = arg.split_once(':') {
        let mut state = State::open();
        if let Some(machine) = state.machine(name) {
            let machine = machine.clone();
            let paths = Paths::of(name, &machine);
            // Relative, from its workdir, as bottle-cli's exec runs there.
            let workdir = machine["workdir"]
                .as_str()
                .unwrap_or("/")
                .trim_end_matches('/');
            let path = match path.trim_start_matches("./") {
                "" | "." => workdir.to_string(),
                path if path.starts_with('/') => path.to_string(),
                path => format!("{workdir}/{path}"),
            };
            return Ok((
                PathBuf::from(paths.here(&path)),
                Some((name.to_string(), paths)),
            ));
        }
    }
    Ok((PathBuf::from(arg), None))
}

/// As bottle-cli's cp: into DST if it's a directory, else as DST; where it went said, NAME:PATH
/// (absolute) if it's in a machine.
fn copy(src: &str, dst: &str) -> ExitCode {
    let result = (|| -> Result<String, String> {
        let (from, from_machine) = side(src)?;
        let (to, to_machine) = side(dst)?;
        if from_machine.is_none() && to_machine.is_none() {
            return Err("cp copies into or out of a machine: name one, MACHINE:PATH".into());
        }
        let name = from
            .file_name()
            .ok_or_else(|| format!("couldn't copy {src}: it sent nothing"))?;
        let target = if to.is_dir() {
            to.join(name)
        } else {
            to.clone()
        };
        copy_tree(&from, &target).map_err(|e| format!("couldn't copy {src} to {dst}: {e}"))?;
        let went = target.display().to_string();
        Ok(match to_machine {
            Some((name, paths)) => format!("{name}:{}", paths.there(&went)),
            None => went,
        })
    })();
    match result {
        Ok(went) => {
            println!("{went}");
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(from, to).map(|_| ())
    }
}
