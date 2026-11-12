//! Reading an OCI image from an archive of its layout (what `buildah push oci-archive:` writes): a
//! tar of `index.json` and `blobs/sha256/`, read in place.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::os::unix::fs::FileExt;
use std::path::Path;

use flate2::read::MultiGzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::other;

/// An OCI layout archive, its blobs found but not read.
pub struct Archive {
    file: File,
    /// Where each blob is in the file, and how long, by digest (`sha256:...`).
    blobs: HashMap<String, (u64, u64)>,
    index: Vec<u8>,
}

/// An image in an archive: its layers, bottom first, and how it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub layers: Vec<Layer>,
    pub config: RunConfig,
}

/// What an image's config says of running it: the part bottle keeps.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RunConfig {
    /// NAME=VALUE.
    #[serde(default)]
    pub env: Option<Vec<String>>,
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default)]
    pub cmd: Option<Vec<String>>,
    #[serde(default)]
    pub labels: Option<std::collections::BTreeMap<String, String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layer {
    /// The digest of the blob, as stored.
    pub digest: String,
    pub media_type: String,
    /// The digest of the layer's tar, uncompressed: what's checked as it's read.
    pub diff_id: String,
}

impl Image {
    pub fn diff_ids(&self) -> Vec<String> {
        self.layers.iter().map(|l| l.diff_id.clone()).collect()
    }
}

#[derive(Deserialize)]
struct Index {
    manifests: Vec<Descriptor>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor {
    media_type: String,
    digest: String,
}

#[derive(Deserialize)]
struct Manifest {
    config: Descriptor,
    layers: Vec<Descriptor>,
}

#[derive(Deserialize)]
struct Config {
    rootfs: RootFs,
    #[serde(default)]
    config: Option<RunConfig>,
}

#[derive(Deserialize)]
struct RootFs {
    diff_ids: Vec<String>,
}

const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";

impl Archive {
    pub fn open(path: &Path) -> io::Result<Archive> {
        let what = |e: io::Error| other(format!("{}: {e}", path.display()));
        let file = File::open(path).map_err(what)?;
        let mut blobs = HashMap::new();
        let mut index = None;
        let mut tar = tar::Archive::new(&file);
        for entry in tar.entries_with_seek().map_err(what)? {
            let mut entry = entry.map_err(what)?;
            let name = entry.path_bytes();
            let name = name.strip_prefix(b"./").unwrap_or(&name);
            if name == b"index.json" {
                let mut json = Vec::new();
                entry.read_to_end(&mut json).map_err(what)?;
                index = Some(json);
            } else if let Some(hex) = name.strip_prefix(b"blobs/sha256/") {
                let digest = format!("sha256:{}", String::from_utf8_lossy(hex));
                blobs.insert(digest, (entry.raw_file_position(), entry.size()));
            }
        }
        let index = index.ok_or_else(|| other(format!("{}: no index.json", path.display())))?;
        Ok(Archive { file, blobs, index })
    }

    /// The archive's one image.
    pub fn image(&self) -> io::Result<Image> {
        let index: Index = parse("index.json", &self.index)?;
        let [descriptor] = &index.manifests[..] else {
            return Err(other(format!(
                "the archive has {} images, not one",
                index.manifests.len()
            )));
        };
        if descriptor.media_type != MANIFEST && descriptor.media_type != DOCKER_MANIFEST {
            return Err(other(format!(
                "the archive's image is a {}, not a manifest",
                descriptor.media_type
            )));
        }
        let manifest = self.small_blob(&descriptor.digest)?;
        let config_digest = parse::<Manifest>("its manifest", &manifest)?.config.digest;
        image_of(&manifest, &self.small_blob(&config_digest)?)
    }

    /// A layer's tar, uncompressed: its end is an error if it isn't what the layer's diff_id says.
    pub fn layer(&self, layer: &Layer) -> io::Result<Box<dyn Read + Send + '_>> {
        decode(self.blob(&layer.digest)?, layer)
    }

    fn blob(&self, digest: &str) -> io::Result<Blob<'_>> {
        let &(pos, len) = self
            .blobs
            .get(digest)
            .ok_or_else(|| other(format!("the archive has no blob {digest}")))?;
        Ok(Blob {
            file: &self.file,
            pos,
            end: pos + len,
        })
    }

    /// A blob read whole, and checked against its digest.
    fn small_blob(&self, digest: &str) -> io::Result<Vec<u8>> {
        let mut data = Vec::new();
        self.blob(digest)?.read_to_end(&mut data)?;
        if sha256(&data) != digest {
            return Err(other(format!("blob {digest} isn't what its digest says")));
        }
        Ok(data)
    }
}

/// The image a manifest and its config say: its layers, and how it runs.
pub fn image_of(manifest: &[u8], config: &[u8]) -> io::Result<Image> {
    let manifest: Manifest = parse("the image's manifest", manifest)?;
    let config: Config = parse("the image's config", config)?;
    let run = config.config.unwrap_or_default();
    let diff_ids = config.rootfs.diff_ids;
    if diff_ids.len() != manifest.layers.len() {
        return Err(other(format!(
            "the image's config has {} layers and its manifest {}",
            diff_ids.len(),
            manifest.layers.len()
        )));
    }
    let layers = manifest
        .layers
        .into_iter()
        .zip(diff_ids)
        .map(|(d, diff_id)| Layer {
            digest: d.digest,
            media_type: d.media_type,
            diff_id,
        })
        .collect();
    Ok(Image {
        layers,
        config: run,
    })
}

/// `blob`, a layer as stored, as its tar, uncompressed: its end is an error if it isn't what the
/// layer's diff_id says.
pub fn decode<'a>(
    blob: impl Read + Send + 'a,
    layer: &Layer,
) -> io::Result<Box<dyn Read + Send + 'a>> {
    let blob = BufReader::with_capacity(1 << 20, blob);
    let tar: Box<dyn Read + Send> = match layer.media_type.as_str() {
        "application/vnd.oci.image.layer.v1.tar"
        | "application/vnd.docker.image.rootfs.diff.tar" => Box::new(blob),
        "application/vnd.oci.image.layer.v1.tar+gzip"
        | "application/vnd.docker.image.rootfs.diff.tar.gzip" => {
            Box::new(MultiGzDecoder::new(blob))
        }
        other_type => {
            return Err(other(format!(
                "layer {}: can't read a {other_type}",
                layer.digest
            )));
        }
    };
    Ok(Box::new(Checked {
        inner: tar,
        hash: Sha256::new(),
        want: layer.diff_id.clone(),
        checked: false,
    }))
}

fn parse<'a, T: Deserialize<'a>>(what: &str, json: &'a [u8]) -> io::Result<T> {
    serde_json::from_slice(json).map_err(|e| other(format!("{what}: {e}")))
}

pub fn sha256(data: &[u8]) -> String {
    hex_digest(Sha256::digest(data))
}

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    let hex: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

/// A blob's bytes in the archive, read where they are.
struct Blob<'a> {
    file: &'a File,
    pos: u64,
    end: u64,
}

impl Read for Blob<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let want = buf.len().min((self.end - self.pos) as usize);
        let n = self.file.read_at(&mut buf[..want], self.pos)?;
        self.pos += n as u64;
        Ok(n)
    }
}

/// A layer's tar, hashed as it's read, and checked at its end.
struct Checked<R> {
    inner: R,
    hash: Sha256,
    want: String,
    checked: bool,
}

impl<R: Read> Read for Checked<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hash.update(&buf[..n]);
        if n == 0 && !buf.is_empty() && !self.checked {
            let got = hex_digest(self.hash.clone().finalize());
            if got != self.want {
                return Err(other(format!(
                    "a layer's tar is {got}, not {} as the image says",
                    self.want
                )));
            }
            self.checked = true;
        }
        Ok(n)
    }
}
