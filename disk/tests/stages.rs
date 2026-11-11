//! An OCI archive written here, merged into stages and written out as the tars mke2fs would get,
//! read back: what's in them, and what's refused. (disk/check/ does it for real, mke2fs, overlayfs
//! and all, but only on Linux, as root.)

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;

use bottle_disk::tree::{View, whiteouts};
use bottle_disk::write::write_stage;
use bottle_disk::{Archive, Image, read_stage};
use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};

fn sha(data: &[u8]) -> String {
    let hex: String = Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256:{hex}")
}

enum E {
    Dir(&'static str),
    File(&'static str, &'static [u8]),
    Link(&'static str, &'static str),
    Sym(&'static str, &'static str),
    Xattr(&'static str, &'static str, &'static [u8]),
}

fn layer(entries: &[E]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for e in entries {
        let mut h = tar::Header::new_gnu();
        h.set_mtime(1_700_000_000);
        h.set_mode(0o644);
        match e {
            E::Dir(p) => {
                h.set_entry_type(tar::EntryType::Directory);
                h.set_mode(0o755);
                h.set_size(0);
                b.append_data(&mut h, p, &[][..]).unwrap();
            }
            E::File(p, data) => {
                h.set_size(data.len() as u64);
                b.append_data(&mut h, p, *data).unwrap();
            }
            E::Link(p, to) | E::Sym(p, to) => {
                let kind = match e {
                    E::Link(..) => tar::EntryType::Link,
                    _ => tar::EntryType::Symlink,
                };
                h.set_entry_type(kind);
                h.set_size(0);
                b.append_link(&mut h, p, to).unwrap();
            }
            E::Xattr(p, name, value) => {
                let key = format!("SCHILY.xattr.{name}");
                b.append_pax_extensions([(key.as_str(), *value)]).unwrap();
                h.set_size(1);
                b.append_data(&mut h, p, &b"x"[..]).unwrap();
            }
        }
    }
    b.into_inner().unwrap()
}

/// An archive of an image of `layers` (tars), written to a file of its own; with each layer's
/// diff_id as given, if Some, rather than its own.
fn archive(name: &str, layers: &[Vec<u8>], diff_ids: Option<Vec<String>>) -> (Archive, Image) {
    let mut blobs = BTreeMap::new();
    let mut descriptors = Vec::new();
    for tar in layers {
        let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
        gz.write_all(tar).unwrap();
        let blob = gz.finish().unwrap();
        descriptors.push(format!(
            r#"{{"mediaType":"application/vnd.oci.image.layer.v1.tar+gzip","digest":"{}","size":{}}}"#,
            sha(&blob),
            blob.len()
        ));
        blobs.insert(sha(&blob), blob);
    }
    let ids = diff_ids.unwrap_or_else(|| layers.iter().map(|t| sha(t)).collect());
    let config = format!(
        r#"{{"rootfs":{{"type":"layers","diff_ids":{}}}}}"#,
        serde_json::to_string(&ids).unwrap()
    )
    .into_bytes();
    let manifest = format!(
        r#"{{"schemaVersion":2,"config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},"layers":[{}]}}"#,
        sha(&config),
        config.len(),
        descriptors.join(",")
    )
    .into_bytes();
    let index = format!(
        r#"{{"schemaVersion":2,"manifests":[{{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"{}","size":{}}}]}}"#,
        sha(&manifest),
        manifest.len()
    );
    blobs.insert(sha(&config), config);
    blobs.insert(sha(&manifest), manifest);
    let mut b = tar::Builder::new(Vec::new());
    let mut add = |path: &str, data: &[u8]| {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        b.append_data(&mut h, path, data).unwrap();
    };
    add("index.json", index.as_bytes());
    for (digest, blob) in &blobs {
        add(&format!("blobs/sha256/{}", &digest[7..]), blob);
    }
    let dir = std::env::temp_dir().join(format!("bottle-disk-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path: PathBuf = dir.join(format!("{name}.tar"));
    fs::write(&path, b.into_inner().unwrap()).unwrap();
    let archive = Archive::open(&path).unwrap();
    let image = archive.image().unwrap();
    (archive, image)
}

/// What the tar of each stage (ending after each of `ends` layers) has: a line per entry, its
/// type, path, link, mode and size.
fn stages(archive: &Archive, image: &Image, ends: &[usize]) -> std::io::Result<Vec<Vec<String>>> {
    let mut view = View::default();
    let mut start = 0;
    let mut out = Vec::new();
    for &end in ends {
        let layers = &image.layers[start..end];
        let stage = read_stage(archive, layers)?;
        let white = whiteouts(&stage, &view);
        let mut tar = Vec::new();
        write_stage(
            &stage,
            &view,
            &white,
            layers.len(),
            |l| archive.layer(&layers[l]),
            &mut tar,
        )?;
        out.push(listing(&tar));
        view.apply(&stage);
        start = end;
    }
    Ok(out)
}

fn listing(tar: &[u8]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut ar = tar::Archive::new(tar);
    for entry in ar.entries().unwrap() {
        let mut e = entry.unwrap();
        let h = e.header().clone();
        let kind = h.entry_type().as_byte() as char;
        let path = String::from_utf8_lossy(&e.path_bytes()).into_owned();
        let link = e
            .link_name_bytes()
            .map(|l| format!(" -> {}", String::from_utf8_lossy(&l)))
            .unwrap_or_default();
        let mut xattrs = String::new();
        if let Some(pax) = e.pax_extensions().unwrap() {
            for r in pax {
                let r = r.unwrap();
                if let Some(name) = r.key().unwrap().strip_prefix("SCHILY.xattr.") {
                    xattrs += &format!(" {name}={:?}", String::from_utf8_lossy(r.value_bytes()));
                }
            }
        }
        let mut data = Vec::new();
        e.read_to_end(&mut data).unwrap();
        let dev = if kind == '3' {
            format!(
                " {},{}",
                h.device_major().unwrap().unwrap(),
                h.device_minor().unwrap().unwrap()
            )
        } else {
            String::new()
        };
        lines.push(format!(
            "{kind} {path}{link} {:o} {}{dev}{xattrs}",
            h.mode().unwrap(),
            data.len()
        ));
    }
    lines
}

#[test]
fn stages_are_written_under_rootfs_with_whiteouts_for_whats_below() {
    let long = format!("d/{}", "n".repeat(150));
    let long: &'static str = Box::leak(long.into_boxed_str());
    let (a, image) = archive(
        "stages",
        &[
            layer(&[
                E::Dir("etc"),
                E::File("etc/a", b"a"),
                E::File("etc/b", b"b"),
                E::Dir("opq"),
                E::File("opq/x", b"x"),
                E::File("hl", b"shared"),
                E::Link("hl2", "hl"),
                E::Dir("d"),
                E::File(long, b"long"),
                E::Sym(
                    "s",
                    "../../a/long/way/back/to/a/target/that/doesnt/fit/in/a/header/at/all/no/not/at/all",
                ),
                E::Xattr("cap", "security.capability", b"\x01\x02"),
            ]),
            layer(&[
                E::File("etc/.wh.a", b""),
                E::Dir("opq"),
                E::File("opq/.wh..wh..opq", b""),
                E::File("opq/y", b"y"),
            ]),
        ],
        None,
    );
    let [bottom, top] = &stages(&a, &image, &[1, 2]).unwrap()[..] else {
        panic!()
    };
    assert_eq!(
        bottom,
        &[
            "5 rootfs 755 0",
            "5 rootfs/etc 755 0",
            "0 rootfs/etc/a 644 1",
            "0 rootfs/etc/b 644 1",
            "5 rootfs/opq 755 0",
            "0 rootfs/opq/x 644 1",
            "0 rootfs/hl 644 6",
            "1 rootfs/hl2 -> rootfs/hl 644 0",
            "5 rootfs/d 755 0",
            &format!("0 rootfs/{long} 644 4"),
            "2 rootfs/s -> ../../a/long/way/back/to/a/target/that/doesnt/fit/in/a/header/at/all/no/not/at/all 777 0",
            "0 rootfs/cap 644 1 security.capability=\"\\u{1}\\u{2}\"",
        ]
    );
    assert_eq!(
        top,
        &[
            "5 rootfs 755 0",
            "5 rootfs/opq 755 0",
            "0 rootfs/opq/y 644 1",
            // No entry of the stage's is in it, only a whiteout: it's written after the layers.
            "5 rootfs/etc 755 0",
            "3 rootfs/etc/a 0 0 0,0",
            "3 rootfs/opq/x 0 0 0,0",
        ]
    );
}

#[test]
fn a_layer_that_isnt_what_its_diff_id_says_is_refused() {
    let tar = layer(&[E::File("a", b"a")]);
    let (a, image) = archive("diff-id", &[tar], Some(vec![sha(b"something else")]));
    let err = stages(&a, &image, &[1]).unwrap_err().to_string();
    assert!(err.contains("not sha256:"), "{err}");
}

#[test]
fn a_hard_link_to_what_a_later_layer_replaced_is_refused() {
    let (a, image) = archive(
        "replaced-link",
        &[
            layer(&[E::File("f", b"old"), E::Link("g", "f")]),
            layer(&[E::File("f", b"new")]),
        ],
        None,
    );
    let err = stages(&a, &image, &[2]).unwrap_err().to_string();
    assert!(err.contains("g: a hard link to f"), "{err}");
    // Split there, the link is in the stage below, as it was, and fine.
    stages(&a, &image, &[1, 2]).unwrap();
}
