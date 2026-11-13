//! `bottle-cli image`, `images` and `gc`: images, stages made in VMs, in a store (see bottle-disk's
//! build); each command that makes one prints the disks of the image it made, bottom first, as
//! `boot` takes them.
//!
//!     bottle-cli image build RECIPE [--tag TAG] [--network FILE | --net-direct]
//!     bottle-cli image import ARCHIVE [--network FILE | --net-direct]
//!     bottle-cli image pull REFERENCE [--network FILE | --net-direct]
//!     bottle-cli image tag IMAGE TAG
//!     bottle-cli image rm TAG
//!     bottle-cli images [--json]
//!     bottle-cli gc [IMAGE]...
//!
//! Its store is $BOTTLE_HOME/stages. RECIPE is a JSON file, steps
//! run on the stage it's `from` (see bottle_disk::build::Recipe); ARCHIVE an OCI layout archive of
//! one image; REFERENCE an image in a registry (`debian:trixie-slim`, `name@sha256:...`), fetched
//! by curl, streamed into the import (see bottle_disk::registry). Images are a store's stages by
//! their tags (Store::tags): `image build` builds a recipe whose base is a registry's image (`pull`:
//! its reference) or one of the store's (`from`: a tag, or a stage's name; a relative mount source
//! is beside RECIPE), and tags what it made; `image tag` and `image rm` name and unname stages;
//! `images` lists the tags. gc keeps every tagged image, every machine's stages, running or not,
//! and IMAGEs'. Each VM is `bottle-cli boot`, this program again, its console kept for when it
//! fails; libkrun and the rest are found as boot finds them, and the network options are boot's: a
//! build's VM has the network they say. A build's cached https hosts are answered with the caching
//! CA builds share (BUILDS_CA), whose certificate a step asking for it (an environment variable
//! BOTTLE_BUILD_CA) is given: an apt step, which trusts it for those hosts.

use std::env;
use std::fs;
use std::io::{self, BufRead, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;

use bottle_disk::Archive;
use bottle_disk::build::{Launch, Mount, Recipe, Store, Vm, wait_for_agent};
use bottle_disk::registry::{Reference, Registry};

use crate::FAILED;
use crate::boot::Net;

/// The caching CA builds' cached hosts are answered with.
const BUILDS_CA: &str = "bottle-builds";
/// What a step asks for the builds' CA's certificate by, in its environment.
const BUILD_CA: &str = "BOTTLE_BUILD_CA";

pub const USAGE: &str = "image build RECIPE [--tag TAG] [--network FILE | --net-direct] \
    | image import ARCHIVE [...] | image pull REFERENCE [...] | image tag IMAGE TAG | image rm TAG \
    | images [--json] | gc [IMAGE]...";

/// The store: $BOTTLE_HOME/stages.
pub fn store() -> PathBuf {
    crate::home().join("stages")
}

/// A build VM's size: builds compile things.
const CPUS: &str = "4";
const MEM_MIB: &str = "4096";

/// One argument, and the network options (see boot's Net), in any order; and the store.
fn one_and_net(args: Vec<String>, what: &str) -> Result<(String, PathBuf, Net), ExitCode> {
    let store = store();
    let mut positional = Vec::new();
    let mut net = Net::None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match net.take(&arg, || args.next()) {
            Some(Ok(())) => {}
            Some(Err(e)) => return Err(crate::usage_error(&e)),
            None => positional.push(arg),
        }
    }
    match <[String; 1]>::try_from(positional) {
        Ok([a]) => Ok((a, store, net)),
        Err(_) => Err(crate::usage_error(what)),
    }
}

fn import(args: Vec<String>) -> ExitCode {
    let (archive, store, net) = match one_and_net(args, "image import needs ARCHIVE") {
        Ok(args) => args,
        Err(code) => return code,
    };
    report((|| {
        let store = store_at(&store)?;
        let archive = Archive::open(Path::new(&archive))?;
        let image = archive.image()?;
        let name = store.import(&image, |l| archive.layer(l), &Boot { net })?;
        store.disks(&name)
    })())
}

fn pull(args: Vec<String>) -> ExitCode {
    let (reference, store, net) = match one_and_net(args, "image pull needs REFERENCE") {
        Ok(args) => args,
        Err(code) => return code,
    };
    report((|| {
        let store = store_at(&store)?;
        let mut registry =
            Registry::new(Reference::parse(&reference)?, &store.dir.join("registry"));
        let image = registry.image()?;
        let name = store.import(&image, |l| registry.layer(l), &Boot { net })?;
        store.disks(&name)
    })())
}

/// `image build|import|pull|tag|rm`: images by their tags.
pub fn image(mut args: impl Iterator<Item = String>) -> ExitCode {
    let command = args.next();
    let mut args: Vec<String> = args.collect();
    if matches!(command.as_deref(), Some("tag" | "rm")) {
        args.push(store().to_string_lossy().into_owned());
    }
    match command.as_deref() {
        Some("build") => image_build(args),
        Some("import") => import(args),
        Some("pull") => pull(args),
        Some("tag") => match <[String; 3]>::try_from(args) {
            Ok([image, tag, store]) => report((|| {
                let store = store_for_reading(Path::new(&store))?;
                let name = store.resolve(&image)?;
                store.tag(&tag, &name)?;
                store.disks(&name)
            })()),
            Err(_) => crate::usage_error("image tag needs IMAGE and TAG"),
        },
        Some("rm") => match <[String; 2]>::try_from(args) {
            Ok([tag, store]) => report((|| {
                let store = store_for_reading(Path::new(&store))?;
                if !store.untag(&tag)? {
                    return Err(io::Error::other(format!("no image tagged {tag}")));
                }
                Ok(vec![])
            })()),
            Err(_) => crate::usage_error("image rm needs TAG"),
        },
        _ => crate::usage_error(USAGE),
    }
}

/// `image build RECIPE [--tag TAG]`: the recipe's base pulled (`pull`) or found (`from`, a tag or a
/// stage), the recipe built on it, and what it made tagged.
fn image_build(args: Vec<String>) -> ExitCode {
    let mut tag = None;
    let mut rest = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--tag" {
            match args.next() {
                Some(value) => tag = Some(value),
                None => return crate::usage_error("--tag needs a value"),
            }
        } else {
            rest.push(arg);
        }
    }
    let (recipe, store, net) = match one_and_net(rest, "image build needs RECIPE") {
        Ok(args) => args,
        Err(code) => return code,
    };
    report((|| {
        let store = store_at(&store)?;
        let json = fs::read(&recipe).map_err(|e| io::Error::other(format!("{recipe}: {e}")))?;
        let mut value: serde_json::Value = serde_json::from_slice(&json)
            .map_err(|e| io::Error::other(format!("{recipe}: {e}")))?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| io::Error::other(format!("{recipe}: not an object")))?;
        let base = match (object.remove("pull"), object.remove("from")) {
            (Some(_), Some(_)) => {
                return Err(io::Error::other(format!(
                    "{recipe}: pull or from, not both"
                )));
            }
            (Some(serde_json::Value::String(reference)), None) => {
                let mut registry =
                    Registry::new(Reference::parse(&reference)?, &store.dir.join("registry"));
                let image = registry.image()?;
                Some(store.import(&image, |l| registry.layer(l), &Boot { net: net.clone() })?)
            }
            (None, Some(serde_json::Value::String(image))) => Some(store.resolve(&image)?),
            (None, None) => None,
            _ => {
                return Err(io::Error::other(format!(
                    "{recipe}: pull and from are strings"
                )));
            }
        };
        give_builds_ca(&mut value, &net)?;
        let path = recipe;
        let mut recipe: Recipe =
            serde_json::from_value(value).map_err(|e| io::Error::other(format!("{path}: {e}")))?;
        recipe.from = base;
        // A mount's relative source is beside the recipe: a recipe sent with what it mounts.
        if let Some(mount) = &mut recipe.mount
            && mount.source.is_relative()
        {
            mount.source = Path::new(&path)
                .parent()
                .unwrap_or(Path::new("."))
                .join(&mount.source);
        }
        let name = store.build(&recipe, &Boot { net })?;
        if let Some(tag) = &tag {
            store.tag(tag, &name)?;
        }
        store.disks(&name)
    })())
}

/// The certificate of the builds' caching CA, for the hosts `net`'s policy terminates, in each of
/// the recipe's steps that asks for it (BUILD_CA in its environment).
fn give_builds_ca(recipe: &mut serde_json::Value, net: &Net) -> io::Result<()> {
    let Some(steps) = recipe.get_mut("steps").and_then(|s| s.as_array_mut()) else {
        return Ok(());
    };
    let asking: Vec<&mut serde_json::Value> = steps
        .iter_mut()
        .filter_map(|step| {
            step.get_mut("env")
                .filter(|env| env.get(BUILD_CA).is_some())
        })
        .collect();
    if asking.is_empty() {
        return Ok(());
    }
    let hosts = match net {
        Net::Policy(path) => crate::boot::policy(path)?.terminated_hosts(),
        _ => vec![],
    };
    let certificate = bottle_agent_client::ca::certificate(BUILDS_CA, &hosts)?.unwrap_or_default();
    for env in asking {
        env[BUILD_CA] = certificate.clone().into();
    }
    Ok(())
}

/// `images [--json]`: the store's tags, each with its stage, disks and labels.
pub fn images(args: impl Iterator<Item = String>) -> ExitCode {
    let args: Vec<String> = args.collect();
    let store = store();
    let json = match args.as_slice() {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => return crate::usage_error(USAGE),
    };
    let listed = (|| -> io::Result<Vec<serde_json::Value>> {
        let store = store_for_reading(&store)?;
        let mut images = Vec::new();
        for (tag, name) in store.tags()? {
            // A tag whose stage is gone (deleted by hand) is listed as such, not an error.
            let (disks, labels) = match store.stage(&name) {
                Ok(stage) => (store.disks(&name)?, stage.labels),
                Err(_) => (vec![], Default::default()),
            };
            images.push(serde_json::json!({
                "tag": tag, "name": name, "disks": disks, "labels": labels,
            }));
        }
        Ok(images)
    })();
    match listed {
        Ok(images) if json => {
            println!("{}", serde_json::Value::Array(images));
            ExitCode::SUCCESS
        }
        Ok(images) => {
            for image in images {
                println!(
                    "{}\t{}",
                    image["tag"].as_str().unwrap_or(""),
                    image["name"].as_str().unwrap_or("")
                );
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("bottle-cli: {e}");
            ExitCode::from(FAILED)
        }
    }
}

/// `gc STORE [IMAGE]...`: deletes the stages in STORE no machine uses, and none of IMAGEs (top
/// stages' names, or disks) or of its tagged images is made of. What a machine uses is safe
/// whether it runs or not.
pub fn gc(args: impl Iterator<Item = String>) -> ExitCode {
    let keep: Vec<String> = args.collect();
    let store = store();
    let result = (|| -> io::Result<Vec<String>> {
        let store = store_for_reading(&store)?;
        let mut kept = std::collections::BTreeSet::new();
        let in_use = crate::machine::disks_in_use()?;
        for name in store.tags()?.values() {
            // A tag whose stage is gone keeps nothing.
            if let Ok(disks) = store.disks(name) {
                kept.extend(disks);
            }
        }
        let named = keep.iter().map(|k| {
            let name = Path::new(k)
                .file_stem()
                .map_or(k.clone(), |s| s.to_string_lossy().into_owned());
            store.disks(&name)
        });
        // One that isn't there has nothing to keep.
        for disks in named.flatten() {
            kept.extend(disks);
        }
        kept.extend(in_use);
        let kept: std::collections::BTreeSet<PathBuf> = kept
            .into_iter()
            .map(|d| fs::canonicalize(&d).unwrap_or(d))
            .collect();
        let mut deleted = Vec::new();
        for name in store.names()? {
            let disk = fs::canonicalize(store.disk(&name)).unwrap_or(store.disk(&name));
            if !kept.contains(&disk) {
                store.remove(&name)?;
                deleted.push(name);
            }
        }
        Ok(deleted)
    })();
    match result {
        Ok(deleted) => {
            for name in deleted {
                println!("deleted {name}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("bottle-cli: {e}");
            ExitCode::from(FAILED)
        }
    }
}

fn store_at(dir: &Path) -> io::Result<Store> {
    fs::create_dir_all(dir)?;
    Ok(Store {
        dir: dir.to_path_buf(),
        mke2fs: crate::mke2fs()?,
    })
}

/// The store at `dir`, for what makes no disk (tags, listing, gc): mke2fs needn't be there.
fn store_for_reading(dir: &Path) -> io::Result<Store> {
    fs::create_dir_all(dir)?;
    Ok(Store {
        dir: dir.to_path_buf(),
        mke2fs: crate::mke2fs().unwrap_or_default(),
    })
}

fn report(disks: io::Result<Vec<PathBuf>>) -> ExitCode {
    match disks {
        Ok(disks) => {
            for disk in disks {
                println!("{}", disk.display());
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("bottle-cli: {e}");
            ExitCode::from(FAILED)
        }
    }
}

/// Boots build VMs with `bottle-cli boot`.
struct Boot {
    net: Net,
}

struct Booted {
    child: Child,
    socket: PathBuf,
    console: PathBuf,
    /// What its boot says, passed on but for each cached request's line, counted instead.
    said: Option<thread::JoinHandle<Downloads>>,
}

/// A build's requests to the hosts it caches, by what became of them (see agent-client's cache).
#[derive(Default)]
struct Downloads {
    kept: usize,
    hit: usize,
    other: usize,
}

/// Passes on what a build's VM's boot says, to this process's stderr, but for the line each request to a
/// cached host makes: a build's output is its steps', and those are counted, for one line at its end.
fn relay(stderr: impl Read) -> Downloads {
    let mut downloads = Downloads::default();
    for line in io::BufReader::new(stderr).lines().map_while(Result::ok) {
        // "bottle-cli: GET https://host/path stored 200"
        let words: Vec<&str> = line.split(' ').collect();
        match words.as_slice() {
            ["bottle-cli:", method, _, outcome, _]
                if method.chars().all(|c| c.is_ascii_uppercase()) =>
            {
                match *outcome {
                    "stored" => downloads.kept += 1,
                    "hit" => downloads.hit += 1,
                    _ => downloads.other += 1,
                }
            }
            _ => eprintln!("{line}"),
        }
    }
    downloads
}

impl Launch for Boot {
    fn launch(
        &self,
        lowers: &[PathBuf],
        upper: &Path,
        mount: Option<&Mount>,
    ) -> io::Result<Box<dyn Vm>> {
        // A unix socket's path has room for only so many characters (104 on macOS): in the
        // temporary directory, not the store.
        static N: AtomicU32 = AtomicU32::new(0);
        let base = env::temp_dir().join(format!(
            "bottle-build-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let socket = base.with_extension("sock");
        let console = base.with_extension("console");
        let child = Command::new(env::current_exe()?)
            .arg("boot")
            .arg(&socket)
            .arg(upper)
            .args(lowers)
            .args(self.net.args())
            .args(match self.net {
                // An image's apt repositories' https answered with the builds' caching CA.
                Net::Policy(_) => vec!["--ca", BUILDS_CA],
                _ => vec![],
            })
            .args(mount.into_iter().flat_map(|m| {
                [
                    "--mount".to_string(),
                    format!("{}:{}", m.source.display(), m.target),
                ]
            }))
            .args(["--cpus", CPUS, "--mem", MEM_MIB, "--console"])
            .arg(&console)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut child = child;
        let said = child
            .stderr
            .take()
            .map(|stderr| thread::spawn(move || relay(stderr)));
        let mut booted = Booted {
            child,
            socket,
            console,
            said,
        };
        let socket = booted.socket.clone();
        // Dropped if it doesn't answer: killed.
        wait_for_agent(&socket, || matches!(booted.child.try_wait(), Ok(None)))
            .map_err(|e| booted.with_console(e))?;
        Ok(Box::new(booted))
    }
}

impl Booted {
    /// `e`, and the end of the VM's console, which says why, if anything does.
    fn with_console(&self, e: io::Error) -> io::Error {
        let console = fs::read_to_string(&self.console).unwrap_or_default();
        let tail: Vec<&str> = console.lines().rev().take(20).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        io::Error::other(format!(
            "{e}\nthe VM's console, at the end:\n{}",
            tail.join("\n")
        ))
    }
}

impl Vm for Booted {
    fn socket(&self) -> &Path {
        &self.socket
    }

    fn wait(mut self: Box<Self>) -> io::Result<()> {
        let status = self.child.wait()?;
        if let Some(Ok(d)) = self.said.take().map(|t| t.join())
            && d.kept + d.hit + d.other > 0
        {
            eprintln!(
                "bottle-cli: cached hosts: {} answered from the cache, {} kept, {} passed on",
                d.hit, d.kept, d.other
            );
        }
        if status.success() {
            Ok(())
        } else {
            Err(self.with_console(io::Error::other(format!("the VM ended so: {status}"))))
        }
    }
}

impl Drop for Booted {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.socket);
        let _ = fs::remove_file(&self.console);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_builds_vm_says_what_it_would_but_each_cached_request_is_counted() {
        let said = "bottle-cli: GET http://deb.example/a.deb stored 200\n\
                    bottle-cli: GET https://deb.example/b.deb hit 200\n\
                    bottle-cli: GET http://deb.example/InRelease passed 200\n\
                    bottle-cli: refused 10.0.0.1:80: not allowed\n";
        let d = relay(said.as_bytes());
        assert_eq!((d.kept, d.hit, d.other), (1, 1, 1));
    }
}
