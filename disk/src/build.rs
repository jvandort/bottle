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
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::thread;
use std::time::{Duration, Instant};

use bottle_agent_client::{Exit, Start, run, stop};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::oci::{Archive, Image};
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
    /// A directory of the host's, copied in to `to`, everything owned by root.
    Copy { copy: PathBuf, to: String },
    /// A command, as root, in `/`, with the image's environment so far and `env`; it must exit 0.
    Run {
        run: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    /// Variables set in the image, for the steps after and for what runs in it.
    Env { env: BTreeMap<String, String> },
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

/// How a build boots its VM: on `lowers` (disks, bottom first), with `upper` on top.
pub trait Launch {
    fn launch(&self, lowers: &[PathBuf], upper: &Path) -> io::Result<Box<dyn Vm>>;
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

    pub fn stage(&self, name: &str) -> io::Result<Stage> {
        let path = self.dir.join(format!("{name}.json"));
        let json = fs::read(&path).map_err(|e| other(format!("{}: {e}", path.display())))?;
        serde_json::from_slice(&json).map_err(|e| other(format!("{}: {e}", path.display())))
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
        self.make(&name, &disks, launch, |socket| {
            for step in &recipe.steps {
                match step {
                    Step::Copy { copy, to } => {
                        let source = copy.clone();
                        unpack(socket, to, false, move |w| write_tree(&source, w))?;
                    }
                    Step::Run {
                        run: argv,
                        env: step_env,
                    } => {
                        let all = env
                            .iter()
                            .chain(step_env)
                            .map(|(k, v)| (k.clone(), v.clone()));
                        command(socket, argv, all.collect())?;
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

    /// Imports the image in `archive`: one stage, its layers applied on nothing. Its name is the
    /// image's, by its layers.
    pub fn import(
        &self,
        archive: &Archive,
        image: &Image,
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
        self.make(&name, &[], launch, |socket| {
            for layer in &image.layers {
                thread::scope(|scope| -> io::Result<()> {
                    let (reader, mut writer) = io::pipe()?;
                    let feeding = scope.spawn(move || -> io::Result<()> {
                        io::copy(&mut archive.layer(layer)?, &mut writer)?;
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

    /// The name of what `recipe` builds: of the stage it's on, its steps, the content of what they
    /// copy (not where it is), and the image's config.
    pub fn name_of(&self, recipe: &Recipe) -> io::Result<String> {
        let mut steps = Vec::new();
        for step in &recipe.steps {
            steps.push(match step {
                Step::Copy { copy, to } => {
                    serde_json::json!({"copy": tree_hash(copy)?, "to": to})
                }
                other => serde_json::to_value(other).unwrap(),
            });
        }
        let inputs = serde_json::json!({
            "from": recipe.from,
            "steps": steps,
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
        steps: impl FnOnce(&Path) -> io::Result<()>,
    ) -> io::Result<()> {
        let partial = self
            .dir
            .join(format!(".{name}.ext4.{}", std::process::id()));
        let _ = fs::remove_file(&partial);
        crate::mke2fs::empty(&self.mke2fs, &partial, DISK_BYTES)?;
        let result = (|| {
            let vm = launch.launch(lowers, &partial)?;
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

/// Runs `argv` with `env`, its output to stderr: what a build shows as it goes.
fn command(socket: &Path, argv: &[String], env: Vec<(String, String)>) -> io::Result<()> {
    let (_, controls) = channel();
    let start = Start {
        argv: argv.to_vec(),
        env,
        cwd: Some("/".into()),
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

/// Unpacks what `write` writes, a tar, into `dir` in the VM (see bottle-agent unpack).
fn unpack(
    socket: &Path,
    dir: &str,
    oci: bool,
    write: impl FnOnce(&mut io::PipeWriter) -> io::Result<()> + Send + 'static,
) -> io::Result<()> {
    let (reader, mut writer) = io::pipe()?;
    let feeding = thread::spawn(move || write(&mut writer));
    let unpacked = unpack_from(socket, dir, oci, reader);
    feeding.join().unwrap().and(unpacked)
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

/// Writes the tree at `dir` as a tar: directories, files and symlinks, owned by root, with their
/// modes and times.
fn write_tree(dir: &Path, out: &mut impl Write) -> io::Result<()> {
    let mut tar = tar::Builder::new(out);
    for (rel, meta, path) in walk(dir)? {
        let mut h = tar::Header::new_gnu();
        h.set_uid(0);
        h.set_gid(0);
        h.set_mode(meta.permissions().mode() & 0o7777);
        h.set_mtime(meta.mtime().max(0) as u64);
        if meta.is_dir() {
            h.set_entry_type(tar::EntryType::Directory);
            h.set_size(0);
            tar.append_data(&mut h, &rel, io::empty())?;
        } else if meta.file_type().is_symlink() {
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            tar.append_link(&mut h, &rel, fs::read_link(&path)?)?;
        } else if meta.is_file() {
            h.set_size(meta.len());
            tar.append_data(&mut h, &rel, fs::File::open(&path)?)?;
        }
    }
    tar.finish()
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
    fn recipes_read_as_written() {
        let recipe: Recipe = serde_json::from_str(
            r#"{"from": "abc", "steps": [
                {"copy": "/host/f", "to": "/tmp/f"},
                {"run": ["sh", "-c", "true"], "env": {"A": "1"}},
                {"run": ["true"]},
                {"env": {"LANG": "C.UTF-8"}}
            ], "cmd": ["sh"]}"#,
        )
        .unwrap();
        assert_eq!(recipe.from.as_deref(), Some("abc"));
        assert_eq!(
            recipe.steps,
            [
                Step::Copy {
                    copy: "/host/f".into(),
                    to: "/tmp/f".into()
                },
                Step::Run {
                    run: vec!["sh".into(), "-c".into(), "true".into()],
                    env: [("A".into(), "1".into())].into()
                },
                Step::Run {
                    run: vec!["true".into()],
                    env: BTreeMap::new()
                },
                Step::Env {
                    env: [("LANG".into(), "C.UTF-8".into())].into()
                },
            ]
        );
        assert!(serde_json::from_str::<Recipe>(r#"{"steps": [{"nope": 1}]}"#).is_err());
    }

    #[test]
    fn a_stage_is_named_for_what_it_copies_not_where_from() {
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
            steps: vec![Step::Copy {
                copy: dir.into(),
                to: "/x".into(),
            }],
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
