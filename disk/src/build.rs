//! Stages made by running things on the stages below: a VM is booted on them with a fresh, empty
//! disk on top, the steps are run in it through the agent, and when it stops, that disk, overlayfs's
//! upper directory, all that changed, is the new stage. Importing an OCI image is the same, its
//! layers applied, in order, on nothing.
//!
//! Stages live in a store, a directory: each `<name>.ext4`, and `<name>.json`, what's below it and
//! how its image runs ([`Stage`]). A stage is named for what made it, so a build of the same, on the
//! same, is the stage there already.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::thread;
use std::time::{Duration, Instant};

use bottle_agent_client::{Exit, Start, run, stop};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::oci::{Image, Layer};
use crate::other;

/// What the names of stages made here are made of, after their inputs: a stage made differently
/// gets a new name.
const FORMAT: &str = "bottle-stage 1";

/// How big a stage's disk is, at most: sparse, what isn't used of it takes no room.
const DISK_BYTES: u64 = 64 << 30;

/// How long a VM gets to start answering.
const BOOT_TIMEOUT: Duration = Duration::from_secs(60);

/// What a build does, on what.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    /// The stage it's built on, by name in the store; on nothing if None.
    #[serde(default)]
    pub from: Option<String>,
    /// A directory of the host's, read-only in the build's VM at its target, for its steps: what
    /// they install from. Not in the stage.
    #[serde(default)]
    pub mount: Option<Mount>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default)]
    pub cmd: Option<Vec<String>>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

/// One thing a build does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Step {
    /// A command, as root, in `cwd` (`/` if None), with the image's environment so far and `env`;
    /// it must exit 0.
    Run {
        run: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    /// Variables set in the image, for the steps after and for what runs in it.
    Env { env: BTreeMap<String, String> },
}

/// A directory of the host's, at `target` in a VM. A target under /run is on its tmpfs, and
/// leaves nothing on the stage's disk; anywhere else, the directory made for it would stay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    pub source: PathBuf,
    pub target: String,
}

/// A stage in the store, as its `.json` has it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stage {
    /// The stages below it, bottom first, by name.
    pub lowers: Vec<String>,
    /// The image's environment, this stage's and those below's.
    pub env: BTreeMap<String, String>,
    pub entrypoint: Option<Vec<String>>,
    pub cmd: Option<Vec<String>>,
    pub labels: BTreeMap<String, String>,
}

/// A VM booted for a build, its agent answering at `socket()`.
pub trait Vm {
    fn socket(&self) -> &Path;
    /// Waits for the VM to end, once it's been told to stop.
    fn wait(self: Box<Self>) -> io::Result<()>;
}

/// How a build boots its VM: on `lowers` (disks, bottom first), with `upper` on top, and
/// `mount`, read-only, if Some.
pub trait Launch {
    fn launch(
        &self,
        lowers: &[PathBuf],
        upper: &Path,
        mount: Option<&Mount>,
    ) -> io::Result<Box<dyn Vm>>;
}

/// A store of stages.
pub struct Store {
    pub dir: PathBuf,
    pub mke2fs: PathBuf,
}

impl Store {
    pub fn disk(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.ext4"))
    }

    /// The names of the stages in the store.
    pub fn names(&self) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let file = entry.file_name();
            let file = file.to_string_lossy();
            if let Some(name) = file.strip_suffix(".json")
                && !name.starts_with('.')
                && self.disk(name).exists()
            {
                names.push(name.to_string());
            }
        }
        names.sort();
        Ok(names)
    }

    /// Deletes stage `name`: its disk, then its json.
    pub fn remove(&self, name: &str) -> io::Result<()> {
        fs::remove_file(self.disk(name))?;
        fs::remove_file(self.dir.join(format!("{name}.json")))
    }

    pub fn stage(&self, name: &str) -> io::Result<Stage> {
        let path = self.dir.join(format!("{name}.json"));
        let json = fs::read(&path).map_err(|e| other(format!("{}: {e}", path.display())))?;
        serde_json::from_slice(&json).map_err(|e| other(format!("{}: {e}", path.display())))
    }

    /// The store's tags: names for its stages, each a stage's name, as `image tag` gave them.
    pub fn tags(&self) -> io::Result<BTreeMap<String, String>> {
        match fs::read(self.tags_path()) {
            Ok(json) => serde_json::from_slice(&json)
                .map_err(|e| other(format!("{}: {e}", self.tags_path().display()))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(other(format!("{}: {e}", self.tags_path().display()))),
        }
    }

    /// Names stage `name` `tag`, instead of whatever it named before.
    pub fn tag(&self, tag: &str, name: &str) -> io::Result<()> {
        if tag.is_empty() || tag.chars().any(char::is_control) {
            return Err(other(format!("{tag:?} isn't a tag")));
        }
        self.stage(name)?;
        self.change_tags(|tags| {
            tags.insert(tag.to_string(), name.to_string());
        })
    }

    /// Removes `tag`, leaving its stages to gc; whether there was one.
    pub fn untag(&self, tag: &str) -> io::Result<bool> {
        let mut was = false;
        self.change_tags(|tags| was = tags.remove(tag).is_some())?;
        Ok(was)
    }

    /// The stage `image` is: a tag's, else a stage of that name.
    pub fn resolve(&self, image: &str) -> io::Result<String> {
        if let Some(name) = self.tags()?.get(image) {
            return Ok(name.clone());
        }
        if self.disk(image).exists() && self.stage(image).is_ok() {
            return Ok(image.to_string());
        }
        Err(other(format!("no image {image} in {}", self.dir.display())))
    }

    fn tags_path(&self) -> PathBuf {
        self.dir.join("tags.json")
    }

    /// Reads the tags, changes them and writes them back, whole, with no other process doing the
    /// same in between (builds tag concurrently).
    fn change_tags(&self, change: impl FnOnce(&mut BTreeMap<String, String>)) -> io::Result<()> {
        let lock = fs::File::create(self.dir.join(".tags.lock"))?;
        // SAFETY: flock on a descriptor this function owns; released when `lock` closes.
        if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut tags = self.tags()?;
        change(&mut tags);
        let partial = self.dir.join(".tags.json.partial");
        fs::write(&partial, serde_json::to_vec_pretty(&tags).map_err(io::Error::other)?)?;
        fs::rename(&partial, self.tags_path())
    }

    /// The disks of stage `name`'s image, bottom first, it last.
    pub fn disks(&self, name: &str) -> io::Result<Vec<PathBuf>> {
        let stage = self.stage(name)?;
        Ok(stage
            .lowers
            .iter()
            .map(|n| self.disk(n))
            .chain([self.disk(name)])
            .collect())
    }

    /// Builds `recipe`, or finds it built: the new stage's name.
    pub fn build(&self, recipe: &Recipe, launch: &dyn Launch) -> io::Result<String> {
        let parent = match &recipe.from {
            Some(name) => Some((name.clone(), self.stage(name)?)),
            None => None,
        };
        let name = self.name_of(recipe)?;
        if self.disk(&name).exists() {
            return Ok(name);
        }
        let (mut lowers, mut env) = match &parent {
            Some((name, stage)) => {
                let mut lowers = stage.lowers.clone();
                lowers.push(name.clone());
                (lowers, stage.env.clone())
            }
            None => (Vec::new(), BTreeMap::new()),
        };
        let disks: Vec<PathBuf> = lowers.iter().map(|n| self.disk(n)).collect();
        let labels = parent
            .as_ref()
            .map(|(_, s)| s.labels.clone())
            .unwrap_or_default()
            .into_iter()
            .chain(recipe.labels.clone())
            .collect();
        let entrypoint = recipe
            .entrypoint
            .clone()
            .or_else(|| parent.as_ref().and_then(|(_, s)| s.entrypoint.clone()));
        let cmd = recipe
            .cmd
            .clone()
            .or_else(|| parent.as_ref().and_then(|(_, s)| s.cmd.clone()));
        self.make(&name, &disks, launch, recipe.mount.as_ref(), |socket| {
            for step in &recipe.steps {
                match step {
                    Step::Run {
                        run: argv,
                        env: step_env,
                        cwd,
                    } => {
                        let all = env
                            .iter()
                            .chain(step_env)
                            .map(|(k, v)| (k.clone(), v.clone()));
                        command(socket, argv, all.collect(), cwd.as_deref())?;
                    }
                    Step::Env { env: set } => env.extend(set.clone()),
                }
            }
            Ok(())
        })?;
        self.finish(
            &name,
            Stage {
                lowers: std::mem::take(&mut lowers),
                env,
                entrypoint,
                cmd,
                labels,
            },
        )?;
        Ok(name)
    }

    /// Imports `image`, each of its layers' tars as `layer` reads it (from an archive, or a
    /// registry): one stage, its layers applied on nothing. Its name is the image's, by its layers:
    /// one there already is the stage, and no layer is read.
    pub fn import<'a>(
        &self,
        image: &Image,
        layer: impl Fn(&Layer) -> io::Result<Box<dyn Read + Send + 'a>> + Sync,
        launch: &dyn Launch,
    ) -> io::Result<String> {
        let diff_ids = image.diff_ids();
        let name = hex(&Sha256::digest(
            format!("{FORMAT}\nimport\n{}\n", crate::chain_id(&diff_ids)).as_bytes(),
        ))[..32]
            .to_string();
        if self.disk(&name).exists() {
            return Ok(name);
        }
        let open = &layer;
        self.make(&name, &[], launch, None, |socket| {
            for layer in &image.layers {
                thread::scope(|scope| -> io::Result<()> {
                    let (reader, mut writer) = io::pipe()?;
                    let feeding = scope.spawn(move || -> io::Result<()> {
                        io::copy(&mut open(layer)?, &mut writer)?;
                        Ok(())
                    });
                    let unpacked = unpack_from(socket, "/", true, reader);
                    let fed = feeding.join().unwrap();
                    // What went wrong reading the layer comes first: unpack failing is only what
                    // followed.
                    fed.and(unpacked)
                })
                .map_err(|e| other(format!("layer {}: {e}", layer.digest)))?;
            }
            Ok(())
        })?;
        let config = &image.config;
        let env = config
            .env
            .iter()
            .flatten()
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        self.finish(
            &name,
            Stage {
                lowers: Vec::new(),
                env,
                entrypoint: config.entrypoint.clone(),
                cmd: config.cmd.clone(),
                labels: config.labels.clone().unwrap_or_default(),
            },
        )?;
        Ok(name)
    }

    /// The name of what `recipe` builds: of the stage it's on, the content of what it mounts (not
    /// where it is), its steps, and the image's config.
    pub fn name_of(&self, recipe: &Recipe) -> io::Result<String> {
        let mount = match &recipe.mount {
            Some(m) => {
                Some(serde_json::json!({"source": tree_hash(&m.source)?, "target": m.target}))
            }
            None => None,
        };
        let inputs = serde_json::json!({
            "from": recipe.from,
            "mount": mount,
            "steps": recipe.steps,
            "entrypoint": recipe.entrypoint,
            "cmd": recipe.cmd,
            "labels": recipe.labels,
        });
        Ok(hex(&Sha256::digest(format!("{FORMAT}\n{inputs}\n").as_bytes()))[..32].to_string())
    }

    /// Makes stage `name`'s disk: a fresh one, on `lowers`, in a VM `launch` boots, `steps` run
    /// through its agent; in place once the VM has stopped clean, not before.
    fn make(
        &self,
        name: &str,
        lowers: &[PathBuf],
        launch: &dyn Launch,
        mount: Option<&Mount>,
        steps: impl FnOnce(&Path) -> io::Result<()>,
    ) -> io::Result<()> {
        let partial = self
            .dir
            .join(format!(".{name}.ext4.{}", std::process::id()));
        let _ = fs::remove_file(&partial);
        crate::mke2fs::empty(&self.mke2fs, &partial, DISK_BYTES)?;
        let result = (|| {
            let vm = launch.launch(lowers, &partial, mount)?;
            let ran = steps(vm.socket());
            // Stopped either way, so the VM doesn't outlive the build.
            let stopped = stop(vm.socket()).map_err(|e| other(format!("stopping: {e}")));
            let ended = vm.wait();
            ran?;
            stopped?;
            ended
        })();
        match result.and_then(|()| fs::rename(&partial, self.disk(name))) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = fs::remove_file(&partial);
                Err(e)
            }
        }
    }

    fn finish(&self, name: &str, stage: Stage) -> io::Result<()> {
        let path = self.dir.join(format!("{name}.json"));
        let json = serde_json::to_vec_pretty(&stage).unwrap();
        fs::write(&path, json).map_err(|e| other(format!("{}: {e}", path.display())))
    }
}

/// Waits for the agent at `socket` to answer, as long as `alive` says the VM is there.
pub fn wait_for_agent(socket: &Path, mut alive: impl FnMut() -> bool) -> io::Result<()> {
    let deadline = Instant::now() + BOOT_TIMEOUT;
    loop {
        let (_, controls) = channel();
        let start = Start {
            argv: vec!["true".into()],
            ..Start::default()
        };
        // Any answer: a stage being made on nothing has no `true` to run, and says so.
        match run(
            socket,
            start,
            io::empty(),
            &mut io::sink(),
            &mut io::sink(),
            controls,
        ) {
            Ok(_) | Err(bottle_agent_client::Error::Agent(_)) => return Ok(()),
            Err(_) => {}
        }
        if !alive() {
            return Err(other("the VM ended before its agent answered".into()));
        }
        if Instant::now() > deadline {
            return Err(other("the VM's agent didn't answer".into()));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Runs `argv` with `env`, in `cwd` (`/` if None), its output to stderr: what a build shows as
/// it goes.
fn command(
    socket: &Path,
    argv: &[String],
    env: Vec<(String, String)>,
    cwd: Option<&str>,
) -> io::Result<()> {
    let (_, controls) = channel();
    let start = Start {
        argv: argv.to_vec(),
        env,
        cwd: Some(cwd.unwrap_or("/").into()),
        ..Start::default()
    };
    let exit = run(
        socket,
        start,
        io::empty(),
        &mut io::stderr(),
        &mut io::stderr(),
        controls,
    )
    .map_err(|e| other(format!("{argv:?}: {e}")))?;
    match exit {
        Exit::Code(0) => Ok(()),
        exit => Err(other(format!("{argv:?}: {exit:?}"))),
    }
}

fn unpack_from(
    socket: &Path,
    dir: &str,
    oci: bool,
    tar: impl Read + Send + 'static,
) -> io::Result<()> {
    let mut argv = vec!["/proc/self/exe".to_string(), "unpack".into(), dir.into()];
    if oci {
        argv.push("--oci".into());
    }
    let (_, controls) = channel();
    let start = Start {
        argv: argv.clone(),
        ..Start::default()
    };
    let exit = run(
        socket,
        start,
        tar,
        &mut io::stderr(),
        &mut io::stderr(),
        controls,
    )
    .map_err(|e| other(format!("unpacking into {dir}: {e}")))?;
    match exit {
        Exit::Code(0) => Ok(()),
        exit => Err(other(format!("unpacking into {dir}: {exit:?}"))),
    }
}

/// Everything under `dir`, sorted, each as its path in it, its metadata and its path.
fn walk(dir: &Path) -> io::Result<Vec<(PathBuf, fs::Metadata, PathBuf)>> {
    let mut found = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(rel) = pending.pop() {
        let mut entries: Vec<_> = fs::read_dir(dir.join(&rel))
            .map_err(|e| other(format!("{}: {e}", dir.join(&rel).display())))?
            .collect::<io::Result<_>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries.iter().rev() {
            let child = rel.join(entry.file_name());
            let meta = fs::symlink_metadata(entry.path())?;
            if meta.is_dir() {
                pending.push(child.clone());
            }
            found.push((child, meta, entry.path()));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

/// A hash of the tree at `dir`: its paths, kinds, whether executable, contents and link targets;
/// not owners or times, which a copy doesn't keep (owners) or doesn't matter (times).
fn tree_hash(dir: &Path) -> io::Result<String> {
    let mut hash = Sha256::new();
    for (rel, meta, path) in walk(dir)? {
        hash.update(rel.as_os_str().as_encoded_bytes());
        hash.update(b"\0");
        if meta.is_dir() {
            hash.update(b"d");
        } else if meta.file_type().is_symlink() {
            hash.update(b"l");
            hash.update(fs::read_link(&path)?.as_os_str().as_encoded_bytes());
        } else {
            hash.update(if meta.permissions().mode() & 0o111 != 0 {
                b"x"
            } else {
                b"f"
            });
            hash.update(Sha256::digest(fs::read(&path)?));
        }
        hash.update(b"\0");
    }
    Ok(hex(&hash.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_names_a_stage_until_its_moved_or_removed() {
        let dir = std::env::temp_dir().join(format!("bottle-tags-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let store = Store {
            dir: dir.clone(),
            mke2fs: "mke2fs".into(),
        };
        for name in ["aaaa", "bbbb"] {
            fs::write(store.disk(name), "").unwrap();
            let stage = serde_json::to_vec(&Stage::default()).unwrap();
            fs::write(dir.join(format!("{name}.json")), stage).unwrap();
        }
        assert!(store.tags().unwrap().is_empty());
        store.tag("localhost/bottle/base:latest", "aaaa").unwrap();
        assert_eq!(store.resolve("localhost/bottle/base:latest").unwrap(), "aaaa");
        assert_eq!(store.resolve("bbbb").unwrap(), "bbbb"); // a stage by its own name
        store.tag("localhost/bottle/base:latest", "bbbb").unwrap();
        assert_eq!(store.resolve("localhost/bottle/base:latest").unwrap(), "bbbb");
        assert!(store.tag("t", "cccc").is_err()); // no such stage
        assert!(store.tag("a\nb", "aaaa").is_err());
        assert!(store.untag("localhost/bottle/base:latest").unwrap());
        assert!(!store.untag("localhost/bottle/base:latest").unwrap());
        assert!(store.resolve("localhost/bottle/base:latest").is_err());
        assert_eq!(store.names().unwrap(), ["aaaa", "bbbb"]); // tags.json is no stage
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn recipes_read_as_written() {
        let recipe: Recipe = serde_json::from_str(
            r#"{"from": "abc", "mount": {"source": "/host/features", "target": "/run/features"},
                "steps": [
                {"run": ["./install.sh"], "env": {"A": "1"}, "cwd": "/run/features/python"},
                {"run": ["true"]},
                {"env": {"LANG": "C.UTF-8"}}
            ], "cmd": ["sh"]}"#,
        )
        .unwrap();
        assert_eq!(recipe.from.as_deref(), Some("abc"));
        assert_eq!(
            recipe.mount,
            Some(Mount {
                source: "/host/features".into(),
                target: "/run/features".into()
            })
        );
        assert_eq!(
            recipe.steps,
            [
                Step::Run {
                    run: vec!["./install.sh".into()],
                    env: [("A".into(), "1".into())].into(),
                    cwd: Some("/run/features/python".into()),
                },
                Step::Run {
                    run: vec!["true".into()],
                    env: BTreeMap::new(),
                    cwd: None,
                },
                Step::Env {
                    env: [("LANG".into(), "C.UTF-8".into())].into()
                },
            ]
        );
        assert!(serde_json::from_str::<Recipe>(r#"{"steps": [{"nope": 1}]}"#).is_err());
    }

    #[test]
    fn a_stage_is_named_for_what_it_mounts_not_where_from() {
        let base = std::env::temp_dir().join(format!("bottle-build-{}", std::process::id()));
        let (a, b) = (base.join("a"), base.join("b"));
        for dir in [&a, &b] {
            fs::create_dir_all(dir.join("sub")).unwrap();
            fs::write(dir.join("sub/f"), "same").unwrap();
        }
        let store = Store {
            dir: base.clone(),
            mke2fs: "mke2fs".into(),
        };
        let recipe = |dir: &Path| Recipe {
            mount: Some(Mount {
                source: dir.into(),
                target: "/run/x".into(),
            }),
            ..Recipe::default()
        };
        assert_eq!(
            store.name_of(&recipe(&a)).unwrap(),
            store.name_of(&recipe(&b)).unwrap()
        );
        fs::write(b.join("sub/f"), "different").unwrap();
        assert_ne!(
            store.name_of(&recipe(&a)).unwrap(),
            store.name_of(&recipe(&b)).unwrap()
        );
        fs::remove_dir_all(&base).unwrap();
    }
}
