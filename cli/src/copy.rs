//! `bottle-cli cp SRC DST`: a file or a directory copied into, out of, or between machines, as
//! cp -r would: into DST if it's a directory, else as DST. MACHINE:PATH is a path in that machine
//! (a relative one from its workdir, as `exec` runs there); any other is a path here, and one side
//! at least is in a machine. A shorthand, as `shell` is, for what `exec` does: a tar, packed by tar
//! on one side and unpacked by it on the other, by way of this machine.
//!
//! In a machine, it's its tar's, as `exec`'s user there. Here, it's unpacked by this machine's tar,
//! which keeps it to where it's unpacked. Either way it's unpacked beside DST first, so nothing is
//! there until all of it is: a copy that fails leaves DST as it was. Says where it went.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};

use crate::FAILED;

pub const USAGE: &str = "cp SRC DST    (either MACHINE:PATH, or a path here)";

/// In a machine, whatever $1 names, as a tar of it alone: its last name, and what's under it.
const PACK: &str = r#"p=$(realpath -e -- "$1") && exec tar -C "$(dirname -- "$p")" -cf - -- "$(basename -- "$p")""#;

/// In a machine, the tar on stdin, of one thing, as cp -r would put it: into $1 if it's a
/// directory, else as $1; unpacked beside it first. Says where it went.
const UNPACK: &str = r#"umask 022
d=$1
if [ -d "$d" ]; then into=$d; else into=$(dirname -- "$d"); fi
s=$(mktemp -d -- "$into/.bottle-cp-XXXXXX") || exit 1
trap 'rm -rf -- "$s"' EXIT
tar -xf - -C "$s" --no-same-owner || exit 1
n=$(ls -A -- "$s")
if [ -z "$n" ] || [ "$(printf '%s\n' "$n" | wc -l)" -ne 1 ]; then echo "it sent nothing" >&2; exit 1; fi
if [ -d "$d" ]; then t=${d%/}/$n; else t=$d; fi
if [ -d "$t" ] && [ ! -L "$t" ]; then echo "$t is a directory already: copy into it, or somewhere new" >&2; exit 1; fi
if [ -d "$s/$n" ] && [ ! -L "$s/$n" ] && { [ -e "$t" ] || [ -L "$t" ]; }; then
  echo "$t exists, and what's copied is a directory" >&2; exit 1
fi
mv -fT -- "$s/$n" "$t" && printf '%s\n' "$t"
"#;

/// One side of a copy.
#[derive(Debug, PartialEq)]
enum Side {
    Here(PathBuf),
    In(String, String),
}

impl Side {
    /// MACHINE:PATH, if what's before the first colon is a machine there is; else a path here (so
    /// ./a:b is one).
    fn parse(arg: &str) -> Side {
        match arg.split_once(':') {
            Some((name, path)) if crate::machine::is_machine(name) => Side::In(
                name.to_string(),
                if path.is_empty() { "." } else { path }.to_string(),
            ),
            _ => Side::Here(arg.into()),
        }
    }
}

pub fn main(args: impl Iterator<Item = String>) -> ExitCode {
    let args: Vec<String> = args.collect();
    let [src, dst] = &args[..] else {
        return crate::usage_error(USAGE);
    };
    match copy(Side::parse(src), Side::parse(dst), src, dst) {
        Ok(to) => {
            println!("{to}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("bottle-cli: {e}");
            ExitCode::from(FAILED)
        }
    }
}

fn other(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

fn copy(src: Side, dst: Side, src_arg: &str, dst_arg: &str) -> io::Result<String> {
    match (src, dst) {
        (Side::Here(_), Side::Here(_)) => Err(other(
            "cp copies into or out of a machine: name one, MACHINE:PATH",
        )),
        (Side::In(_, path), _) if path.trim_end_matches('/').is_empty() => {
            Err(other("cp copies a file or a directory, not /"))
        }
        (Side::Here(from), Side::In(machine, to)) => {
            if fs::symlink_metadata(&from).is_err() {
                return Err(other(format!(
                    "couldn't copy {src_arg}: there's no such file or directory"
                )));
            }
            // Its last name, and where it is: what `..` or `.` name, if it's that.
            let (parent, base) = match from.file_name() {
                Some(name) => (
                    from.parent()
                        .filter(|p| !p.as_os_str().is_empty())
                        .map_or_else(|| PathBuf::from("."), Path::to_path_buf),
                    PathBuf::from(name),
                ),
                None => {
                    let resolved = fs::canonicalize(&from)?;
                    match (resolved.parent(), resolved.file_name()) {
                        (Some(parent), Some(name)) => (parent.to_path_buf(), PathBuf::from(name)),
                        _ => return Err(other("cp copies a file or a directory, not /")),
                    }
                }
            };
            let mut pack = crate::packing_tar()
                .arg("-C")
                .arg(&parent)
                .args(["-cf", "-", "--"])
                .arg(&base)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            let unpack = in_machine(
                &machine,
                UNPACK,
                &to,
                Stdio::from(pack.stdout.take().unwrap()),
            )?;
            let (taken, (out, said)) = finish(unpack)?;
            let (sent, (_, sent_said)) = finish(pack)?;
            // What it was taken by says why first: tar here ends when it does.
            if !taken {
                return Err(other(format!(
                    "couldn't copy {src_arg} to {dst_arg}: {}",
                    or(&said, "it failed")
                )));
            }
            if !sent {
                return Err(other(format!(
                    "couldn't copy {src_arg}: {}",
                    or(&sent_said, "tar failed")
                )));
            }
            Ok(format!("{machine}:{}", out.trim()))
        }
        (Side::In(machine, from), Side::Here(to)) => {
            let into = to.is_dir();
            let beside = if into {
                to.clone()
            } else {
                to.parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| ".".into())
            };
            let scratch = Scratch::new(&beside)?;
            let pack = in_machine(&machine, PACK, &from, Stdio::null())?;
            let mut unpack = Command::new("tar")
                .arg("-xf")
                .arg("-")
                .arg("-C")
                .arg(&scratch.0)
                .arg("--no-same-owner")
                .stdin(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()?;
            let mut pack = pack;
            let mut out = pack.stdout.take().unwrap();
            match io::copy(&mut out, unpack.stdin.as_mut().unwrap()) {
                // tar here ends at the archive's end, before its padding: what's left is drained.
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                    io::copy(&mut out, &mut io::sink())?;
                }
                result => {
                    result?;
                }
            }
            drop(unpack.stdin.take());
            let (sent, said) = finish(pack)?;
            let unpacked = unpack.wait()?.success();
            let names: Vec<_> = fs::read_dir(&scratch.0)?.collect::<Result<_, _>>()?;
            if !sent || !unpacked || names.len() != 1 {
                return Err(other(format!(
                    "couldn't copy {src_arg}: {}",
                    or(&said.1, "it sent nothing")
                )));
            }
            let copied = names[0].path();
            let target = if into {
                to.join(names[0].file_name())
            } else {
                to
            };
            let is_dir = |p: &Path| fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
            if is_dir(&target) {
                return Err(other(format!(
                    "{} is a directory already: copy into it, or somewhere new",
                    target.display()
                )));
            }
            if is_dir(&copied) && fs::symlink_metadata(&target).is_ok() {
                return Err(other(format!(
                    "{} exists, and {src_arg} is a directory",
                    target.display()
                )));
            }
            fs::rename(&copied, &target)?;
            Ok(target.display().to_string())
        }
        (Side::In(from_machine, from), Side::In(to_machine, to)) => {
            let mut pack = in_machine(&from_machine, PACK, &from, Stdio::null())?;
            let unpack = in_machine(
                &to_machine,
                UNPACK,
                &to,
                Stdio::from(pack.stdout.take().unwrap()),
            )?;
            let (taken, said) = finish(unpack)?;
            let (sent, sent_said) = finish(pack)?;
            if !sent {
                return Err(other(format!(
                    "couldn't copy {src_arg}: {}",
                    or(&sent_said.1, "it sent nothing")
                )));
            }
            if !taken {
                return Err(other(format!(
                    "couldn't copy {src_arg} to {dst_arg}: {}",
                    or(&said.1, "it failed")
                )));
            }
            Ok(format!("{to_machine}:{}", said.0.trim()))
        }
    }
}

/// `script` run in `machine` by `exec`, with `path` as its $1 and `stdin`: its stdout and stderr
/// piped here.
fn in_machine(machine: &str, script: &str, path: &str, stdin: Stdio) -> io::Result<Child> {
    Command::new(std::env::current_exe()?)
        .args(["exec", machine, "--", "sh", "-c", script, "sh", path])
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

/// Whether `child` succeeded, with its stdout (what's left of it) and stderr.
fn finish(child: Child) -> io::Result<(bool, (String, String))> {
    let out = child.wait_with_output()?;
    Ok((
        out.status.success(),
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ),
    ))
}

fn or<'a>(said: &'a str, otherwise: &'a str) -> &'a str {
    if said.is_empty() { otherwise } else { said }
}

/// A directory of its own beside where a copy's going, removed with what's left in it.
struct Scratch(PathBuf);

impl Scratch {
    fn new(beside: &Path) -> io::Result<Scratch> {
        let mut bytes = [0u8; 6];
        fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        let tag: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let dir = beside.join(format!(".bottle-cp-{tag}"));
        fs::create_dir(&dir).map_err(|e| other(format!("{}: {e}", dir.display())))?;
        Ok(Scratch(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_colon_names_a_machine_only_if_there_is_one() {
        assert_eq!(Side::parse("./a:b"), Side::Here("./a:b".into()));
        assert_eq!(Side::parse("nope:x"), Side::Here("nope:x".into()));
    }
}
