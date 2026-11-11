//! An OCI image made into the disks a bottle's VM boots from: one ext4 per stage of the image, each
//! stage the layers that one image of its FROM chain adds to the one below (debian, then bottle's
//! base, then a feature bag on that), for overlayfs in the guest to stack, read-only, under a
//! bottle's own writable disk. Every bottle of images that share a stage shares that stage's disk,
//! the same file, and so the host's cache of it.
//!
//! A disk is named for what's in it: the layers of its stage and those below, by their diff_ids.
//! What's made already is used as it is. Its files are under `rootfs/`.
//!
//! mke2fs makes the filesystem, from a tar of the stage written to it; it's a program of its own,
//! run, never linked (see dist/mke2fs.sh).

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

pub mod mke2fs;
pub mod oci;
pub mod tree;
pub mod write;

pub use oci::{Archive, Image, Layer};

use tree::{Meta, Stage, View};

/// What the disks' names are made of, after the layers: a disk made differently gets a new name.
const FORMAT: &str = "bottle-disk 1";

fn other(message: String) -> io::Error {
    io::Error::other(message)
}

/// The disks of `image`, in `archive`, in `dir`, bottom first, made with `mke2fs` if they aren't
/// there: one for each stage, the stages ending after each of `splits` layers and at the top.
pub fn make_disks(
    archive: &Archive,
    image: &Image,
    splits: &[usize],
    dir: &Path,
    mke2fs: &Path,
) -> io::Result<Vec<PathBuf>> {
    let n = image.layers.len();
    let mut ends: Vec<usize> = splits.iter().copied().filter(|&s| s > 0 && s < n).collect();
    ends.push(n);
    ends.sort();
    ends.dedup();
    let diff_ids = image.diff_ids();
    let mut view = View::default();
    let mut disks = Vec::new();
    let mut start = 0;
    for &end in &ends {
        let name = disk_name(&diff_ids[..start], &diff_ids[..end]);
        let path = dir.join(format!("{}.ext4", &name[..32]));
        let layers = &image.layers[start..end];
        let top = end == n;
        // The stages below the top are read even when their disks are there: what's above
        // needs to know what's in them.
        if !(top && path.exists()) {
            let stage = read_stage(archive, layers)?;
            if !path.exists() {
                make_disk(archive, layers, &stage, &view, &path, &name, mke2fs)?;
            }
            if !top {
                view.apply(&stage);
            }
        }
        disks.push(path);
        start = end;
    }
    Ok(disks)
}

/// Where `image` splits into stages: after the layers of each of `bases` (images it's built on),
/// whose layers must be the first of its own.
pub fn splits(image: &Image, bases: &[Image]) -> io::Result<Vec<usize>> {
    let ids = image.diff_ids();
    bases
        .iter()
        .map(|base| {
            let theirs = base.diff_ids();
            if ids.starts_with(&theirs) {
                Ok(theirs.len())
            } else {
                Err(other(
                    "a base image's layers aren't the first of the image's".into(),
                ))
            }
        })
        .collect()
}

/// The stage of `layers`, merged.
pub fn read_stage(archive: &Archive, layers: &[Layer]) -> io::Result<Stage> {
    let mut stage = Stage::default();
    for (l, layer) in layers.iter().enumerate() {
        let what = |e: io::Error| other(format!("layer {}: {e}", layer.digest));
        read_layer(&mut stage, l, archive.layer(layer)?).map_err(what)?;
    }
    Ok(stage)
}

/// Adds the layer whose tar is `tar` to `stage`, as its `l`th.
pub fn read_layer(stage: &mut Stage, l: usize, tar: impl Read) -> io::Result<()> {
    let mut tar = tar::Archive::new(tar);
    for (i, entry) in tar.entries()?.enumerate() {
        let mut entry = entry?;
        let Some(path) = write::path_of(&entry)? else {
            continue;
        };
        let meta = meta(&mut entry)
            .map_err(|e| other(format!("{}: {e}", String::from_utf8_lossy(&path))))?;
        stage.add((l, i), &path, meta)?;
    }
    // The rest of the tar, which its check is at the end of.
    io::copy(&mut tar.into_inner(), &mut io::sink())?;
    Ok(())
}

fn meta<R: Read>(entry: &mut tar::Entry<R>) -> io::Result<Meta> {
    let mut pax = Vec::new();
    if let Some(records) = entry.pax_extensions()? {
        for record in records {
            let record = record?;
            let key = record.key_bytes();
            if key.starts_with(b"GNU.sparse.") {
                return Err(other(
                    "a sparse file in PAX's form, which isn't read".into(),
                ));
            }
            if ![&b"path"[..], b"linkpath", b"size"].contains(&key) {
                pax.push((key.to_vec(), record.value_bytes().to_vec()));
            }
        }
    }
    let h = entry.header();
    let kind = match h.entry_type().as_byte() {
        // Contiguous, and GNU's sparse, which the tar crate reads whole.
        b'\0' | b'7' | b'S' => b'0',
        k @ (b'0'..=b'6') => k,
        k => {
            return Err(other(format!("an entry of type {:?}", k as char)));
        }
    };
    Ok(Meta {
        kind,
        mode: h.mode()? & 0o7777,
        uid: h.uid()?,
        gid: h.gid()?,
        mtime: h.mtime()?,
        uname: h.username_bytes().unwrap_or_default().to_vec(),
        gname: h.groupname_bytes().unwrap_or_default().to_vec(),
        // Some tars leave the fields blank for what isn't a device.
        dev: match kind {
            b'3' | b'4' => (
                h.device_major()?.unwrap_or(0),
                h.device_minor()?.unwrap_or(0),
            ),
            _ => (0, 0),
        },
        size: if kind == b'0' { entry.size() } else { 0 },
        link: entry.link_name_bytes().unwrap_or_default().into_owned(),
        pax,
    })
}

fn make_disk(
    archive: &Archive,
    layers: &[Layer],
    stage: &Stage,
    view: &View,
    path: &Path,
    name: &str,
    mke2fs: &Path,
) -> io::Result<()> {
    let whiteouts = tree::whiteouts(stage, view);
    let size = mke2fs::size(stage, whiteouts.len());
    let file = path.file_name().unwrap().to_string_lossy();
    let partial = path.with_file_name(format!(".{file}.{}", std::process::id()));
    let _ = fs::remove_file(&partial);
    let seed: [u8; 16] = Sha256::digest(name.as_bytes())[..16].try_into().unwrap();
    let made = mke2fs::make(mke2fs, &partial, size, &seed, |out| {
        write::write_stage(
            stage,
            view,
            &whiteouts,
            layers.len(),
            |l| archive.layer(&layers[l]),
            out,
        )
    });
    match made.and_then(|()| fs::rename(&partial, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&partial);
            Err(other(format!("making {}: {e}", path.display())))
        }
    }
}

/// The name of the disk of the layers `upper` has past `lower`: hex, made of their chain IDs.
pub fn disk_name(lower: &[String], upper: &[String]) -> String {
    let text = format!("{FORMAT}\n{}\n{}\n", chain_id(lower), chain_id(upper));
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The OCI chain ID of `diff_ids`, which names them all in order; empty for none.
pub fn chain_id(diff_ids: &[String]) -> String {
    let mut chain = String::new();
    for id in diff_ids {
        chain = if chain.is_empty() {
            id.clone()
        } else {
            oci::sha256(format!("{chain} {id}").as_bytes())
        };
    }
    chain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chain_id_is_the_first_diff_id_then_each_hashed_on() {
        let ids = ["sha256:a".to_string(), "sha256:b".into()];
        assert_eq!(chain_id(&ids[..1]), "sha256:a");
        assert_eq!(chain_id(&ids), oci::sha256(b"sha256:a sha256:b"));
        assert_eq!(chain_id(&[]), "");
    }

    #[test]
    fn a_disk_is_named_for_where_its_stage_starts_too() {
        let ids = ["sha256:a".to_string(), "sha256:b".into()];
        assert_ne!(disk_name(&ids[..1], &ids), disk_name(&[], &ids));
    }
}
