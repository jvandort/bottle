//! `bottle-cli build`, `import` and `pull`: stages made in VMs, in a store (see bottle-disk's
//! build), each printing the disks of the image it made, bottom first, as `boot` takes them; and
//! `gc`, which deletes the stages nothing needs.
//!
//!     bottle-cli build RECIPE STORE [--network FILE | --net-direct]
//!     bottle-cli import ARCHIVE STORE [--network FILE | --net-direct]
//!     bottle-cli pull REFERENCE STORE [--network FILE | --net-direct]
//!     bottle-cli gc STORE [IMAGE]...
//!     bottle-cli image build RECIPE STORE [--tag TAG] [--network FILE | --net-direct]
//!     bottle-cli image tag IMAGE TAG STORE
//!     bottle-cli image rm TAG STORE
//!     bottle-cli images STORE [--json]
//!
//! RECIPE is a JSON file, steps run on the stage it's `from` (see bottle_disk::build::Recipe);
//! ARCHIVE an OCI layout archive of one image; REFERENCE an image in a registry
//! (`debian:trixie-slim`, `name@sha256:...`), fetched by curl, streamed into the import (see
//! bottle_disk::registry). Images are a store's stages by their tags (Store::tags): `image build`
//! builds a recipe whose base is a registry's image (`pull`: its reference) or one of the store's
//! (`from`: a tag, or a stage's name; a relative mount source is beside RECIPE), and tags what
//! it made; `image tag` and `image rm` name and
//! unname stages; `images` lists the tags. gc keeps every tagged image, every machine's stages,
//! running or not, and IMAGEs'. Each VM
//! is `bottle-cli boot`, this program again, its console kept for when it fails; libkrun and the
//! rest are found as boot finds them, and the network options are boot's: a build's VM has the
//! network they say.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

use bottle_disk::Archive;
use bottle_disk::build::{Launch, Mount, Recipe, Store, Vm, wait_for_agent};
use bottle_disk::registry::{Reference, Registry};

use crate::FAILED;
use crate::boot::Net;

/// The caching CA builds' cached hosts are answered with: bottle/ca.py's BUILDS.
const BUILDS_CA: &str = "bottle-builds";

pub const BUILD_USAGE: &str = "build RECIPE STORE [--network FILE | --net-direct]";
pub const IMPORT_USAGE: &str = "import ARCHIVE STORE [--network FILE | --net-direct]";
pub const PULL_USAGE: &str = "pull REFERENCE STORE [--network FILE | --net-direct]";
pub const GC_USAGE: &str = "gc STORE [IMAGE]...";
pub const IMAGE_USAGE: &str = "image build RECIPE STORE [--tag TAG] [--network FILE | --net-direct] \
    | image tag IMAGE TAG STORE | image rm TAG STORE";
pub const IMAGES_USAGE: &str = "images STORE [--json]";

/// A build VM's size: builds compile things.
const CPUS: &str = "4";
const MEM_MIB: &str = "4096";

/// Two arguments, and the network options (see boot's Net), in any order.
fn two_and_net(
    args: impl Iterator<Item = String>,
    what: &str,
) -> Result<(String, String, Net), ExitCode> {
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
    match <[String; 2]>::try_from(positional) {
        Ok([a, b]) => Ok((a, b, net)),
        Err(_) => Err(crate::usage_error(what)),
    }
}

pub fn build(args: impl Iterator<Item = String>) -> ExitCode {
    let (recipe, store, net) = match two_and_net(args, "build needs RECIPE and STORE") {
        Ok(args) => args,
        Err(code) => return code,
    };
    report((|| {
        let store = store_at(Path::new(&store))?;
        let json = fs::read(&recipe).map_err(|e| io::Error::other(format!("{recipe}: {e}")))?;
        let recipe: Recipe = serde_json::from_slice(&json)
            .map_err(|e| io::Error::other(format!("{recipe}: {e}")))?;
        let name = store.build(&recipe, &Boot { net })?;
        store.disks(&name)
    })())
}

pub fn import(args: impl Iterator<Item = String>) -> ExitCode {
    let (archive, store, net) = match two_and_net(args, "import needs ARCHIVE and STORE") {
        Ok(args) => args,
        Err(code) => return code,
    };
    report((|| {
        let store = store_at(Path::new(&store))?;
        let archive = Archive::open(Path::new(&archive))?;
        let image = archive.image()?;
        let name = store.import(&image, |l| archive.layer(l), &Boot { net })?;
        store.disks(&name)
    })())
}

pub fn pull(args: impl Iterator<Item = String>) -> ExitCode {
    let (reference, store, net) = match two_and_net(args, "pull needs REFERENCE and STORE") {
        Ok(args) => args,
        Err(code) => return code,
    };
    report((|| {
        let store = store_at(Path::new(&store))?;
        let mut registry =
            Registry::new(Reference::parse(&reference)?, &store.dir.join("registry"));
        let image = registry.image()?;
        let name = store.import(&image, |l| registry.layer(l), &Boot { net })?;
        store.disks(&name)
    })())
}

/// `image build|tag|rm`: images by their tags.
pub fn image(mut args: impl Iterator<Item = String>) -> ExitCode {
    match args.next().as_deref() {
        Some("build") => image_build(args),
        Some("tag") => match <[String; 3]>::try_from(args.collect::<Vec<_>>()) {
            Ok([image, tag, store]) => report((|| {
                let store = store_for_reading(Path::new(&store))?;
                let name = store.resolve(&image)?;
                store.tag(&tag, &name)?;
                store.disks(&name)
            })()),
            Err(_) => crate::usage_error("image tag needs IMAGE, TAG and STORE"),
        },
        Some("rm") => match <[String; 2]>::try_from(args.collect::<Vec<_>>()) {
            Ok([tag, store]) => report((|| {
                let store = store_for_reading(Path::new(&store))?;
                if !store.untag(&tag)? {
                    return Err(io::Error::other(format!("no image tagged {tag}")));
                }
                Ok(vec![])
            })()),
            Err(_) => crate::usage_error("image rm needs TAG and STORE"),
        },
        _ => crate::usage_error("image takes build, tag or rm"),
    }
}

/// `image build RECIPE STORE [--tag TAG]`: the recipe's base pulled (`pull`) or found (`from`, a
/// tag or a stage), the recipe built on it, and what it made tagged.
fn image_build(args: impl Iterator<Item = String>) -> ExitCode {
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
    let (recipe, store, net) =
        match two_and_net(rest.into_iter(), "image build needs RECIPE and STORE") {
            Ok(args) => args,
            Err(code) => return code,
        };
    report((|| {
        let store = store_at(Path::new(&store))?;
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

/// `images STORE [--json]`: the store's tags, each with its stage, disks and labels.
pub fn images(args: impl Iterator<Item = String>) -> ExitCode {
    let args: Vec<String> = args.collect();
    let (store, json) = match args.as_slice() {
        [store] => (store, false),
        [store, flag] | [flag, store] if flag == "--json" => (store, true),
        _ => return crate::usage_error(IMAGES_USAGE),
    };
    let listed = (|| -> io::Result<Vec<serde_json::Value>> {
        let store = store_for_reading(Path::new(store))?;
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
pub fn gc(mut args: impl Iterator<Item = String>) -> ExitCode {
    let Some(store) = args.next() else {
        return crate::usage_error("gc needs STORE");
    };
    let keep: Vec<String> = args.collect();
    let result = (|| -> io::Result<Vec<String>> {
        let store = store_for_reading(Path::new(&store))?;
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
            .spawn()?;
        let mut booted = Booted {
            child,
            socket,
            console,
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
