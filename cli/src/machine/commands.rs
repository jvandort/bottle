//! The machine commands: create, start, configure, rename, status, exec, delete, info.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::{fs, io, thread};

use bottle_disk::build::Store;
use bottle_formats::listing;

use crate::FAILED;

use super::lifecycle::Machine;
use super::{Config, Image, USAGE, machines, other};

pub(super) fn create(machine: &Machine, args: &[String]) -> io::Result<()> {
    let mut image = None;
    let mut config = Config {
        image: Vec::new(),
        cpus: 2,
        mem_mib: 2048,
        storage_gib: 64,
        env: Vec::new(),
        mounts: Vec::new(),
        writable: Vec::new(),
        run: None,
        user: None,
        workdir: None,
        labels: Default::default(),
    };
    let mut network = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .cloned()
                .ok_or_else(|| other(format!("{arg} needs a value")))
        };
        let number = |v: String| {
            v.parse()
                .map_err(|_| other(format!("{arg} {v}: not a number")))
        };
        match arg.as_str() {
            "--image" => image = Some(value()?),
            // As many as the hypervisor has for it, at most: boot sees to that.
            "--cpus" => config.cpus = number(value()?).map(|n: u64| n.min(255) as u8)?,
            "--mem" => config.mem_mib = number(value()?)? as u32,
            "--storage" => config.storage_gib = number(value()?)?,
            "-e" => {
                let pair = value()?;
                let (k, v) = pair
                    .split_once('=')
                    .ok_or_else(|| other(format!("-e {pair}: not NAME=VALUE")))?;
                config.env.push((k.into(), v.into()));
            }
            "--user" => config.user = Some(value()?),
            "--workdir" => config.workdir = Some(value()?),
            // Refused now, not when it's started.
            "--network" => network = Some(crate::boot::policy_given(&value()?)?),
            "--label" => {
                let pair = value()?;
                let (k, v) = pair
                    .split_once('=')
                    .ok_or_else(|| other(format!("--label {pair}: not NAME=VALUE")))?;
                config.labels.insert(k.into(), v.into());
            }
            // Read-only, unless it says :rw.
            "-v" => {
                let given = value()?;
                let (spec, writable) = match given.strip_suffix(":rw") {
                    Some(spec) => (spec, true),
                    None => (given.strip_suffix(":ro").unwrap_or(&given), false),
                };
                let (source, target) = spec
                    .rsplit_once(':')
                    .ok_or_else(|| other(format!("-v {given}: not SOURCE:TARGET[:ro|:rw]")))?;
                config
                    .mounts
                    .push((fs::canonicalize(source)?, target.into()));
                if writable {
                    config.writable.push(target.into());
                }
            }
            other_arg => return Err(other(format!("unknown option {other_arg}"))),
        }
    }
    let image = image.ok_or_else(|| other("no --image"))?;
    if machine.exists() {
        return Err(other(format!("there's a machine {} already", machine.name)));
    }
    let (disks, image_env, run) = image_of(&disk_of(&image)?)?;
    // The image's environment first: a machine's own -e overrides it.
    config.env.splice(0..0, image_env);
    (config.image, config.run) = (disks, run);
    let mke2fs = crate::mke2fs()?;
    fs::create_dir_all(&machine.dir)?;
    let made = (|| {
        bottle_disk::mke2fs::empty(&mke2fs, &machine.disk(), config.storage_gib << 30)?;
        if let Some(network) = &network {
            fs::write(machine.network(), network)?;
        }
        fs::write(
            machine.dir.join("config.json"),
            serde_json::to_vec_pretty(&config).unwrap(),
        )
    })();
    if made.is_err() {
        let _ = fs::remove_dir_all(&machine.dir);
    }
    made
}

/// The top stage's disk of `image`: a stage's disk as given, else a tag in the store.
pub(super) fn disk_of(image: &str) -> io::Result<PathBuf> {
    if Path::new(image).is_file() {
        return Ok(image.into());
    }
    let dir = crate::store();
    let store = Store {
        dir: dir.clone(),
        mke2fs: PathBuf::new(),
    };
    let name = store.resolve(image).map_err(|e| {
        other(format!(
            "{image}: not a disk, nor an image in {}: {e}",
            dir.display()
        ))
    })?;
    store
        .disks(&name)?
        .pop()
        .ok_or_else(|| other(format!("{image} has no stages")))
}

/// The disks of the image whose top stage's disk is `disk`, bottom first, its environment, and what
/// it runs (its entrypoint, then its command): its store says, if it's in one; else it's an image
/// of one stage, with no environment, that runs nothing.
pub(super) fn image_of(disk: &Path) -> io::Result<Image> {
    let disk = fs::canonicalize(disk).map_err(|e| other(format!("{}: {e}", disk.display())))?;
    let (Some(dir), Some(name)) = (disk.parent(), disk.file_stem().and_then(|s| s.to_str())) else {
        return Ok((vec![disk], Vec::new(), None));
    };
    if !dir.join(format!("{name}.json")).exists() {
        return Ok((vec![disk], Vec::new(), None));
    }
    let store = Store {
        dir: dir.to_path_buf(),
        mke2fs: PathBuf::new(),
    };
    let stage = store.stage(name)?;
    let run: Vec<String> = stage
        .entrypoint
        .into_iter()
        .flatten()
        .chain(stage.cmd.into_iter().flatten())
        .collect();
    Ok((
        store.disks(name)?,
        stage.env.into_iter().collect(),
        Some(run).filter(|r| !r.is_empty()),
    ))
}

/// `start NAME [--net-direct]`: the network is its policy's, if it has one; else the start's.
pub(super) fn start(machine: &Machine, args: &[String]) -> io::Result<()> {
    let mut net = crate::boot::Net::None;
    let mut given = args.iter();
    while let Some(arg) = given.next() {
        match net.take(arg, || given.next().cloned()) {
            Some(taken) => taken.map_err(other)?,
            None => return Err(other(format!("unknown option {arg}"))),
        }
    }
    machine.start(&net)
}

/// The disks of every machine's image, running or not: what `gc` keeps.
pub fn disks_in_use() -> io::Result<Vec<PathBuf>> {
    let mut disks = Vec::new();
    let Ok(entries) = fs::read_dir(machines()) else {
        return Ok(disks);
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(String::from) else {
            continue;
        };
        if let Ok(machine) = Machine::new(&name)
            && machine.exists()
        {
            disks.extend(machine.config()?.image);
        }
    }
    Ok(disks)
}

pub(super) fn delete(machine: &Machine, args: &[String]) -> io::Result<()> {
    machine.must_exist()?;
    if let Some(arg) = args.iter().find(|a| *a != "-f") {
        return Err(other(format!("unknown option {arg}")));
    }
    if args.iter().any(|a| a == "-f") {
        if let Some(pid) = machine.pid() {
            machine.kill(pid);
        }
    } else {
        machine.stop()?;
    }
    let _ = fs::remove_file(machine.socket());
    fs::remove_dir_all(&machine.dir)?;
    // Its caching CA too (see bottle-net's ca): another machine of its name gets a new one.
    bottle_net::ca::forget(&machine.name)
}

/// `configure NAME --network FILE|-`: a stopped machine's network policy, given again (its
/// creator's, when what it was made with changes).
pub(super) fn configure(machine: &Machine, args: &[String]) -> io::Result<()> {
    let network = match args {
        [flag, network] if flag == "--network" => network,
        _ => return Err(other("configure takes --network FILE|-")),
    };
    machine.must_exist()?;
    if machine.running() {
        return Err(other(format!("{} is running: stop it first", machine.name)));
    }
    let network = crate::boot::policy_given(network)?;
    let staged = machine.dir.join(".network.json");
    fs::write(&staged, network)?;
    fs::rename(staged, machine.network())
}

/// `rename NAME NEW`: a stopped machine, under another name; its disk, config and logs with it.
pub(super) fn rename(machine: &Machine, args: &[String]) -> io::Result<()> {
    let [to] = args else {
        return Err(other("rename takes the machine's new name"));
    };
    let new = Machine::new(to)?;
    machine.must_exist()?;
    if machine.running() {
        return Err(other(format!("{} is running: stop it first", machine.name)));
    }
    if new.exists() || new.dir.exists() {
        return Err(other(format!("there's a machine {} already", new.name)));
    }
    let _ = fs::remove_file(machine.dir.join("pid"));
    fs::rename(&machine.dir, &new.dir)?;
    // Its caching CA is its old name's: one is made for the new name as it's next started.
    bottle_net::ca::forget(&machine.name)
}

pub(super) fn status(machine: &Machine, args: &[String]) -> io::Result<()> {
    if let Some(arg) = args.iter().find(|a| *a != "--json") {
        return Err(other(format!("unknown option {arg}")));
    }
    let state = machine
        .state()
        .ok_or_else(|| other(format!("no machine {}", machine.name)))?;
    if args.iter().any(|a| a == "--json") {
        let status = listing::Status {
            name: machine.name.clone(),
            state,
        };
        println!(
            "{}",
            serde_json::to_string(&status).map_err(io::Error::other)?
        );
    } else {
        println!("{state}");
    }
    Ok(())
}

/// `exec NAME`: `exec --socket` on the machine's socket. -i is taken and ignored, stdin being always
/// passed on.
pub(super) fn exec(machine: &Machine, rest: Vec<String>) -> ExitCode {
    if machine.state().is_none() {
        eprintln!("bottle-cli: no machine {}", machine.name);
        return ExitCode::from(FAILED);
    }
    if !machine.running() {
        eprintln!("bottle-cli: {} isn't running", machine.name);
        return ExitCode::from(FAILED);
    }
    // Its creator's user and directory, unless the command says.
    let options = &rest[..rest.iter().position(|a| a == "--").unwrap_or(rest.len())];
    let given = |flag: &str| options.iter().any(|a| a == flag);
    let mut defaults = Vec::new();
    if let Ok(config) = machine.config() {
        if let Some(user) = config.user.filter(|_| !given("-u")) {
            defaults.extend(["-u".to_string(), user]);
        }
        if let Some(dir) = config.workdir.filter(|_| !given("-w")) {
            defaults.extend(["-w".to_string(), dir]);
        }
    }
    let socket = machine.socket().to_string_lossy().into_owned();
    crate::exec::main(std::iter::once(socket).chain(defaults).chain(rest))
}

/// `info [--json]`: this machine's memory (MiB) and cores, and where bottle's things are on it
/// ($BOTTLE_HOME).
pub fn info(args: impl Iterator<Item = String>) -> ExitCode {
    let args: Vec<String> = args.collect();
    if !(args.is_empty() || args == ["--json"]) {
        return crate::usage_error(USAGE);
    }
    // SAFETY: sysconf has no preconditions.
    let (pages, page) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    let memory_mib = (pages.max(0) as u64 * page.max(0) as u64) >> 20;
    let cpus = thread::available_parallelism().map_or(1, |n| n.get());
    let home = crate::home();
    if args.is_empty() {
        println!(
            "memory  {memory_mib} MiB\ncpus    {cpus}\nhome    {}",
            home.display()
        );
    } else {
        let info = listing::Info {
            memory_mib,
            cpus: cpus as u64,
            home,
        };
        println!("{}", serde_json::to_string(&info).unwrap());
    }
    ExitCode::SUCCESS
}
