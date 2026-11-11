//! A stage written out as the tar mke2fs makes its disk of: everything under `rootfs/`, so that
//! mke2fs's own lost+found stays out of the image.
//!
//! The layers are read again, in order, each entry that survived the merge written as it comes,
//! from its layer's tar, the directories it's in first; then the whiteouts. The tar is written
//! here rather than with the tar crate's builder, which rewrites link targets and refuses some
//! paths: it's PAX, path and link names in its records whenever they don't fit the header.

use std::collections::HashSet;
use std::io::{self, Read, Write};

use crate::other;
use crate::tree::{Below, Kind, Meta, Stage, View, normalize, split};

/// The directory of the disk that has the stage's files.
pub const ROOT: &[u8] = b"rootfs";

/// Writes `stage`, over `view`, to `out`, reading its layers' tars from `layer` (by their place in
/// the stage) again. `whiteouts` are those [`crate::tree::whiteouts`] found.
pub fn write_stage<R: Read>(
    stage: &Stage,
    view: &View,
    whiteouts: &[Vec<u8>],
    layers: usize,
    mut layer: impl FnMut(usize) -> io::Result<R>,
    out: impl Write,
) -> io::Result<()> {
    let mut w = Writer {
        tar: Tar { out },
        stage,
        view,
        written: HashSet::new(),
    };
    w.dir(b"")?;
    for l in 0..layers {
        let mut tar = tar::Archive::new(layer(l)?);
        for (i, entry) in tar.entries()?.enumerate() {
            let mut entry = entry?;
            let Some(path) = path_of(&entry)? else {
                continue;
            };
            let Some(node) = stage.nodes.get(&path) else {
                continue;
            };
            if node.at != Some((l, i)) {
                continue;
            }
            match &node.kind {
                Kind::Dir { .. } => w.dir(&path)?,
                Kind::Hardlink { target, of } => w.hardlink(&path, target, *of)?,
                Kind::Other => {
                    let meta = node.meta.as_ref().unwrap();
                    w.parent(&path)?;
                    w.tar
                        .entry(&rooted(&path), meta, meta.kind, &[], &mut entry)?;
                    w.written.insert(path);
                }
                Kind::Whiteout => {}
            }
        }
        // The rest of the tar, which its check is at the end of.
        io::copy(&mut tar.into_inner(), &mut io::sink())?;
    }
    for (path, node) in &stage.nodes {
        if matches!(node.kind, Kind::Dir { .. }) {
            w.dir(path)?;
        }
    }
    for path in whiteouts {
        w.parent(path)?;
        let device = Meta {
            kind: b'3',
            mode: 0,
            ..Meta::dir()
        };
        w.tar
            .entry(&rooted(path), &device, b'3', &[], &mut io::empty())?;
    }
    w.tar.finish()
}

/// An entry's path, normalized; None for what isn't a file, like a PAX global header.
pub fn path_of<R: Read>(entry: &tar::Entry<R>) -> io::Result<Option<Vec<u8>>> {
    match entry.header().entry_type().as_byte() {
        b'g' => Ok(None),
        _ => normalize(&entry.path_bytes()).map(Some),
    }
}

struct Writer<'a, W> {
    tar: Tar<W>,
    stage: &'a Stage,
    view: &'a View,
    written: HashSet<Vec<u8>>,
}

impl<W: Write> Writer<'_, W> {
    /// Writes the directory at `path`, and those it's in, unless they're written already: as the
    /// stage's layers last describe it, or else as the stages below do.
    fn dir(&mut self, path: &[u8]) -> io::Result<()> {
        if self.written.contains(path) {
            return Ok(());
        }
        self.parent(path)?;
        let meta = match (self.stage.nodes.get(path), self.view.get(path)) {
            (
                Some(crate::tree::Node {
                    meta: Some(meta), ..
                }),
                _,
            ) => meta.clone(),
            (_, Some(Below::Dir(meta))) => meta.clone(),
            _ => Meta::dir(),
        };
        self.tar
            .entry(&rooted(path), &meta, b'5', &[], &mut io::empty())?;
        self.written.insert(path.to_vec());
        Ok(())
    }

    fn parent(&mut self, path: &[u8]) -> io::Result<()> {
        match split(path) {
            Some((dir, _)) => self.dir(dir),
            None => Ok(()),
        }
    }

    fn hardlink(
        &mut self,
        path: &[u8],
        target: &[u8],
        of: Option<crate::tree::Pos>,
    ) -> io::Result<()> {
        let node = self.stage.nodes.get(path).unwrap();
        let ok = of.is_some()
            && self.written.contains(target)
            && self.stage.nodes.get(target).is_some_and(|t| t.at == of);
        if !ok {
            // The file it linked to is in a stage below, or a later layer replaced it: either way
            // the content is somewhere this tar can't point at.
            return Err(other(format!(
                "{}: a hard link to {}, which isn't there in its stage as it was",
                String::from_utf8_lossy(path),
                String::from_utf8_lossy(target)
            )));
        }
        self.parent(path)?;
        let meta = node.meta.as_ref().unwrap();
        self.tar
            .entry(&rooted(path), meta, b'1', &rooted(target), &mut io::empty())?;
        self.written.insert(path.to_vec());
        Ok(())
    }
}

fn rooted(path: &[u8]) -> Vec<u8> {
    if path.is_empty() {
        return ROOT.to_vec();
    }
    [ROOT, b"/", path].concat()
}

const BLOCK: usize = 512;

/// The largest number each octal field of a header holds.
const MAX_ID: u64 = 0o7777777;
const MAX_SIZE: u64 = 0o77777777777;

struct Tar<W> {
    out: W,
}

impl<W: Write> Tar<W> {
    /// Writes an entry of type `kind`: `meta`'s, but the path, link and type given here. A file's
    /// `data` is `meta.size` long; a hard link's `link` is a path in the tar.
    fn entry(
        &mut self,
        path: &[u8],
        meta: &Meta,
        kind: u8,
        link: &[u8],
        data: &mut dyn Read,
    ) -> io::Result<()> {
        let link = if kind == b'2' { &meta.link[..] } else { link };
        let size = if kind == b'0' { meta.size } else { 0 };
        let mut records = Vec::new();
        for (key, value) in &meta.pax {
            record(&mut records, key, value);
        }
        if path.len() > 100 {
            record(&mut records, b"path", path);
        }
        if link.len() > 100 {
            record(&mut records, b"linkpath", link);
        }
        if size > MAX_SIZE {
            record(&mut records, b"size", size.to_string().as_bytes());
        }
        for (key, id) in [(&b"uid"[..], meta.uid), (b"gid", meta.gid)] {
            if id > MAX_ID && !meta.pax.iter().any(|(k, _)| k == key) {
                record(&mut records, key, id.to_string().as_bytes());
            }
        }
        if !records.is_empty() {
            let pax = Meta {
                kind: b'x',
                mode: 0o644,
                size: records.len() as u64,
                ..Meta::dir()
            };
            self.header(b"././@PaxHeader", &pax, b'x', &[], records.len() as u64)?;
            self.out.write_all(&records)?;
            self.pad(records.len() as u64)?;
        }
        self.header(path, meta, kind, link, size)?;
        let copied = io::copy(&mut data.take(size), &mut self.out)?;
        if copied != size {
            return Err(other(format!(
                "{}: {copied} bytes of {size}",
                String::from_utf8_lossy(path)
            )));
        }
        self.pad(size)
    }

    fn header(
        &mut self,
        path: &[u8],
        meta: &Meta,
        kind: u8,
        link: &[u8],
        size: u64,
    ) -> io::Result<()> {
        let mut h = [0u8; BLOCK];
        put(&mut h[0..100], path);
        // A symlink's mode means nothing, and anything that makes one makes it 0777.
        let mode = if kind == b'2' {
            0o777
        } else {
            meta.mode & 0o7777
        };
        octal(&mut h[100..108], u64::from(mode));
        octal(
            &mut h[108..116],
            if meta.uid > MAX_ID { 0 } else { meta.uid },
        );
        octal(
            &mut h[116..124],
            if meta.gid > MAX_ID { 0 } else { meta.gid },
        );
        octal(&mut h[124..136], if size > MAX_SIZE { 0 } else { size });
        octal(&mut h[136..148], meta.mtime.min(MAX_SIZE));
        h[148..156].fill(b' ');
        h[156] = kind;
        put(&mut h[157..257], link);
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        put(&mut h[265..297], &meta.uname[..meta.uname.len().min(31)]);
        put(&mut h[297..329], &meta.gname[..meta.gname.len().min(31)]);
        octal(&mut h[329..337], u64::from(meta.dev.0));
        octal(&mut h[337..345], u64::from(meta.dev.1));
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        let digits = format!("{sum:06o}\0 ");
        h[148..156].copy_from_slice(digits.as_bytes());
        self.out.write_all(&h)
    }

    fn pad(&mut self, len: u64) -> io::Result<()> {
        let rem = (len % BLOCK as u64) as usize;
        if rem != 0 {
            self.out.write_all(&[0; BLOCK][rem..])?;
        }
        Ok(())
    }

    fn finish(&mut self) -> io::Result<()> {
        self.out.write_all(&[0; 2 * BLOCK])?;
        self.out.flush()
    }
}

/// As much of `value` as fits in `field`.
fn put(field: &mut [u8], value: &[u8]) {
    let n = value.len().min(field.len());
    field[..n].copy_from_slice(&value[..n]);
}

/// `n` in octal, zero-padded to fill `field` but its last byte, a NUL.
fn octal(field: &mut [u8], n: u64) {
    let width = field.len() - 1;
    let digits = format!("{n:0width$o}");
    field[..width].copy_from_slice(&digits.as_bytes()[digits.len() - width..]);
    field[width] = 0;
}

/// A PAX record, "LEN KEY=VALUE\n", LEN counting itself.
fn record(out: &mut Vec<u8>, key: &[u8], value: &[u8]) {
    let rest = 1 + key.len() + 1 + value.len() + 1;
    let mut len = rest + 1;
    while len != rest + len.to_string().len() {
        len = rest + len.to_string().len();
    }
    out.extend_from_slice(len.to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(key);
    out.push(b'=');
    out.extend_from_slice(value);
    out.push(b'\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pax_records_count_their_own_length() {
        let mut out = Vec::new();
        record(&mut out, b"path", b"x");
        assert_eq!(out, b"9 path=x\n");
        let mut out = Vec::new();
        record(&mut out, b"k", &[b'v'; 93]);
        assert_eq!(out.len(), 99);
        assert!(out.starts_with(b"99 k="));
        // One more, and the length has a digit more too.
        let mut out = Vec::new();
        record(&mut out, b"k", &[b'v'; 94]);
        assert_eq!(out.len(), 101);
        assert!(out.starts_with(b"101 k="));
    }

    #[test]
    fn octal_fills_its_field() {
        let mut f = [0xffu8; 8];
        octal(&mut f, 0o755);
        assert_eq!(&f, b"0000755\0");
    }
}
