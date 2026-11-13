//! `bottle-cli cp SRC DST`: a file or a directory copied into, out of, or between machines, as
//! cp -r would: into DST if it's a directory, else as DST. MACHINE:PATH is a path in that machine
//! (a relative one from its workdir, as `exec` runs there); `-` is a tar of it on stdin (SRC) or
//! stdout (DST), as another's cp makes or takes it (another machine's bottle-cli's, over ssh); any
//! other is a path here. One side at least is in a machine, or `-`. A shorthand, as `shell` is,
//! for what `exec` does: a tar, packed by tar on one side and unpacked by it on the other, by way
//! of this machine.
//!
//! In a machine, it's its tar's, as `exec`'s user there. Here, it's unpacked by this machine's tar,
//! which keeps it to where it's unpacked. Either way it's unpacked beside DST first, so nothing is
//! there until all of it is: a copy that fails leaves DST as it was. Says where it went.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};

use crate::FAILED;

pub const USAGE: &str =
    "cp SRC DST    (either MACHINE:PATH, - for a tar on stdin or stdout, or a path here)";

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
    /// A tar, on stdin or stdout.
    Stream,
}

impl Side {
    /// `-`; MACHINE:PATH, if what's before the first colon is a machine there is; else a path here
    /// (so ./a:b is one).
    fn parse(arg: &str) -> Side {
        if arg == "-" {
            return Side::Stream;
        }
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
    let dst_side = Side::parse(dst);
    let streamed = dst_side == Side::Stream;
    match copy(Side::parse(src), dst_side, src, dst) {
        // Where it went, but for a tar on stdout, which is all that's there.
        Ok(to) => {
            if !streamed {
                println!("{to}");
            }
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

/// Where a copy comes from: a tar, packed by a process here (this machine's tar, or a machine's by
/// `exec`), or on stdin.
enum Source {
    Packed(Child),
    Stdin,
}

impl Source {
    /// It, as what's unpacked by a process reads it.
    fn stdio(&mut self) -> Stdio {
        match self {
            Source::Packed(pack) => Stdio::from(pack.stdout.take().unwrap()),
            Source::Stdin => Stdio::inherit(),
        }
    }

    /// It, as this process reads it.
    fn reader(&mut self) -> Box<dyn Read> {
        match self {
            Source::Packed(pack) => Box::new(pack.stdout.take().unwrap()),
            Source::Stdin => Box::new(io::stdin()),
        }
    }

    /// Whether it packed what it was asked, and what it said: so, for stdin, whoever's sent it.
    fn finish(self) -> io::Result<(bool, String)> {
        match self {
            Source::Packed(pack) => finish(pack).map(|(sent, (_, said))| (sent, said)),
            Source::Stdin => Ok((true, String::new())),
        }
    }
}

fn copy(src: Side, dst: Side, src_arg: &str, dst_arg: &str) -> io::Result<String> {
    match (&src, &dst) {
        (Side::Here(_), Side::Here(_)) => {
            return Err(other(
                "cp copies into or out of a machine: name one, MACHINE:PATH",
            ));
        }
        (Side::Stream, Side::Stream) => return Err(other("cp copies one tar, not to another")),
        (Side::In(_, path), _) if path.trim_end_matches('/').is_empty() => {
            return Err(other("cp copies a file or a directory, not /"));
        }
        _ => {}
    }
    let mut source = match &src {
        Side::Here(from) => Source::Packed(pack_here(from, src_arg)?),
        Side::In(machine, from) => Source::Packed(in_machine(machine, PACK, from, Stdio::null())?),
        Side::Stream => Source::Stdin,
    };
    match dst {
        Side::In(machine, to) => {
            let unpack = in_machine(&machine, UNPACK, &to, source.stdio())?;
            let (taken, (out, said)) = finish(unpack)?;
            let (sent, sent_said) = source.finish()?;
            // What sent it says why first, but for this machine's tar, which ends when what it
            // sends to does.
            let first = matches!(src, Side::Here(_));
            if !sent && !first {
                return Err(other(format!(
                    "couldn't copy {src_arg}: {}",
                    or(&sent_said, "it sent nothing")
                )));
            }
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
        Side::Here(to) => {
            let mut reader = source.reader();
            let unpacked = unpack_here(&mut reader, &to, src_arg);
            drop(reader);
            let (sent, said) = source.finish()?;
            match unpacked {
                Ok(target) if sent => Ok(target),
                Err(e) if sent => Err(e),
                _ => Err(other(format!(
                    "couldn't copy {src_arg}: {}",
                    or(&said, "it sent nothing")
                ))),
            }
        }
        Side::Stream => {
            let mut reader = source.reader();
            io::copy(&mut reader, &mut io::stdout())?;
            drop(reader);
            let (sent, said) = source.finish()?;
            if !sent {
                return Err(other(format!(
                    "couldn't copy {src_arg}: {}",
                    or(&said, "it sent nothing")
                )));
            }
            Ok(String::new())
        }
    }
}

/// This machine's tar of `from`, a file or a directory here: its last name, and what's under it.
fn pack_here(from: &Path, src_arg: &str) -> io::Result<Child> {
    if fs::symlink_metadata(from).is_err() {
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
            let resolved = fs::canonicalize(from)?;
            match (resolved.parent(), resolved.file_name()) {
                (Some(parent), Some(name)) => (parent.to_path_buf(), PathBuf::from(name)),
                _ => return Err(other("cp copies a file or a directory, not /")),
            }
        }
    };
    crate::packing_tar()
        .arg("-C")
        .arg(&parent)
        .args(["-cf", "-", "--"])
        .arg(&base)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

/// The tar `reader` reads, of one thing, unpacked here as cp -r would put it: into `to` if it's a
/// directory, else as `to`; beside it first. Where it went.
fn unpack_here(reader: &mut dyn Read, to: &Path, src_arg: &str) -> io::Result<String> {
    let into = to.is_dir();
    let beside = if into {
        to.to_path_buf()
    } else {
        to.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| ".".into())
    };
    let scratch = Scratch::new(&beside)?;
    let mut unpack = Command::new("tar")
        .arg("-xf")
        .arg("-")
        .arg("-C")
        .arg(&scratch.0)
        .arg("--no-same-owner")
        .stdin(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    match io::copy(reader, unpack.stdin.as_mut().unwrap()) {
        // tar here ends at the archive's end, before its padding: what's left is drained.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
            io::copy(reader, &mut io::sink())?;
        }
        result => {
            result?;
        }
    }
    drop(unpack.stdin.take());
    let unpacked = unpack.wait()?.success();
    let names: Vec<_> = fs::read_dir(&scratch.0)?.collect::<Result<_, _>>()?;
    if !unpacked || names.len() != 1 {
        return Err(other(format!("couldn't copy {src_arg}: it sent nothing")));
    }
    let copied = names[0].path();
    let target = if into {
        to.join(names[0].file_name())
    } else {
        to.to_path_buf()
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
        assert_eq!(Side::parse("-"), Side::Stream);
        assert_eq!(Side::parse("./-"), Side::Here("./-".into()));
    }
}
