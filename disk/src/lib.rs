//! Images as the disks a bottle's VM boots from: one ext4 per stage of an image, each holding
//! what its stage changed, for overlayfs in the guest to stack, read-only, under a bottle's own
//! writable disk. Every bottle of images that share a stage shares that stage's disk, the same
//! file, and so the host's cache of it.
//!
//! A stage is made in a VM (see build): booted on the stages below with a fresh disk on top, its
//! steps run, that disk is the stage, overlayfs's upper directory, all that changed. An OCI image
//! is imported the same way, its layers applied on nothing (see oci, for reading its archive).

use std::io;

pub mod build;
pub mod mke2fs;
pub mod oci;
pub mod registry;

pub use oci::{Archive, Image, Layer};

fn other(message: String) -> io::Error {
    io::Error::other(message)
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
}
