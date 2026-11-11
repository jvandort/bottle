//! Layers merged into a stage, and the stages below seen as one.
//!
//! A stage is some of an image's layers, applied one on another as the OCI image spec says, into
//! what overlayfs takes as one of its lower directories: what the stage's layers delete from the
//! stages below stays, as a whiteout, and a directory they replace is opaque, its contents below
//! hidden. Paths are bytes, as in the tars: relative to the image's root, without `.` or empty
//! components; the root is the empty path.

use std::collections::BTreeMap;
use std::io;

use crate::other;

/// An entry's place in a stage: its layer's, counted from the stage's first, then its own in the
/// layer's tar.
pub type Pos = (usize, usize);

/// What a tar entry says of a file, apart from its path and contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    /// The tar's type: b'0' a file, b'1' a hard link, b'2' a symlink, b'3' and b'4' character
    /// and block devices, b'5' a directory, b'6' a fifo.
    pub kind: u8,
    /// Permissions, setuid, setgid and sticky.
    pub mode: u32,
    pub uid: u64,
    pub gid: u64,
    pub mtime: u64,
    pub uname: Vec<u8>,
    pub gname: Vec<u8>,
    pub dev: (u32, u32),
    /// A file's; 0 for anything else.
    pub size: u64,
    /// A symlink's target as it is, or a hard link's, as a path in the image.
    pub link: Vec<u8>,
    /// The entry's PAX records but its path, link and size: xattrs, and anything else the
    /// header's fields can't hold.
    pub pax: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Meta {
    /// A directory no layer describes: root's, 0755.
    pub fn dir() -> Meta {
        Meta {
            kind: b'5',
            mode: 0o755,
            uid: 0,
            gid: 0,
            mtime: 0,
            uname: Vec::new(),
            gname: Vec::new(),
            dev: (0, 0),
            size: 0,
            link: Vec::new(),
            pax: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A directory; opaque if the stage's layers removed what was there before making it, so
    /// that nothing below shows through.
    Dir { opaque: bool },
    /// Something the stage's layers delete from those below.
    Whiteout,
    /// A hard link to `target`, which was the entry at `of` (if the stage had it) when the link
    /// was made.
    Hardlink { target: Vec<u8>, of: Option<Pos> },
    /// A file, symlink, device or fifo.
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub kind: Kind,
    /// None for a whiteout, or a directory the stage's layers have things in but don't describe.
    pub meta: Option<Meta>,
    /// The entry that says what this is, if one does.
    pub at: Option<Pos>,
    /// The last layer with this, or something in it.
    layer: usize,
}

/// A stage's layers, merged.
#[derive(Debug, Default)]
pub struct Stage {
    pub nodes: BTreeMap<Vec<u8>, Node>,
}

const OPAQUE: &[u8] = b".wh..wh..opq";
const WHITEOUT: &[u8] = b".wh.";

impl Stage {
    /// Applies the entry at `at`, as `path`.
    pub fn add(&mut self, at: Pos, path: &[u8], meta: Meta) -> io::Result<()> {
        let layer = at.0;
        let Some((parent, name)) = split(path) else {
            if meta.kind != b'5' {
                return Err(other("a layer's root isn't a directory".into()));
            }
            self.touch_dirs(path, layer);
            let root = self.nodes.get_mut(path).unwrap();
            root.meta = Some(meta);
            root.at = Some(at);
            return Ok(());
        };
        self.touch_dirs(parent, layer);
        if name == OPAQUE {
            self.remove(parent, false, Some(layer));
            self.nodes.get_mut(parent).unwrap().kind = Kind::Dir { opaque: true };
            return Ok(());
        }
        if let Some(hidden) = name.strip_prefix(WHITEOUT) {
            // Whiteouts delete from the layers before theirs, never from their own.
            let target = join(parent, hidden);
            self.remove(&target, true, Some(layer));
            match self.nodes.get_mut(&target) {
                Some(Node {
                    kind: Kind::Dir { opaque },
                    ..
                }) => *opaque = true,
                Some(_) => {}
                None => {
                    self.nodes.insert(
                        target,
                        Node {
                            kind: Kind::Whiteout,
                            meta: None,
                            at: None,
                            layer,
                        },
                    );
                }
            }
            return Ok(());
        }
        let is_dir = meta.kind == b'5';
        if let Some(node) = self.nodes.get_mut(path)
            && is_dir
            && matches!(node.kind, Kind::Dir { .. })
        {
            node.meta = Some(meta);
            node.at = Some(at);
            node.layer = layer;
            return Ok(());
        }
        let replaced = self.nodes.contains_key(path);
        self.remove(path, true, None);
        let kind = match meta.kind {
            b'5' => Kind::Dir { opaque: replaced },
            b'1' => {
                let target = normalize(&meta.link)?;
                let of = self.nodes.get(&target).and_then(|n| n.at);
                Kind::Hardlink { target, of }
            }
            _ => Kind::Other,
        };
        self.nodes.insert(
            path.to_vec(),
            Node {
                kind,
                meta: Some(meta),
                at: Some(at),
                layer,
            },
        );
        Ok(())
    }

    /// Makes `dir` and each directory it's in a directory that `layer` has something in.
    fn touch_dirs(&mut self, dir: &[u8], layer: usize) {
        for end in prefixes(dir) {
            let path = &dir[..end];
            match self.nodes.get_mut(path) {
                Some(Node {
                    kind: Kind::Dir { .. },
                    layer: l,
                    ..
                }) => *l = layer,
                found => {
                    // What was there, a file or a whiteout, hides what's below.
                    let opaque = found.is_some();
                    self.remove(path, true, None);
                    self.nodes.insert(
                        path.to_vec(),
                        Node {
                            kind: Kind::Dir { opaque },
                            meta: None,
                            at: None,
                            layer,
                        },
                    );
                }
            }
        }
    }

    /// Removes what's in `path`, and `path` itself if `inclusive`; only what no layer from
    /// `before` on has touched, if Some.
    fn remove(&mut self, path: &[u8], inclusive: bool, before: Option<usize>) {
        let doomed: Vec<Vec<u8>> = subtree(&self.nodes, path, inclusive)
            .filter(|(_, node)| before.is_none_or(|layer| node.layer < layer))
            .map(|(p, _)| p.clone())
            .collect();
        for path in doomed {
            self.nodes.remove(&path);
        }
    }
}

/// What a path below is: a directory and what describes it, or something else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Below {
    Dir(Meta),
    Other,
}

/// The stages below one, as overlayfs shows them stacked.
#[derive(Debug)]
pub struct View {
    nodes: BTreeMap<Vec<u8>, Below>,
}

impl Default for View {
    fn default() -> View {
        View {
            nodes: BTreeMap::from([(Vec::new(), Below::Dir(Meta::dir()))]),
        }
    }
}

impl View {
    pub fn get(&self, path: &[u8]) -> Option<&Below> {
        self.nodes.get(path)
    }

    /// The paths of what's in the directory at `dir`, in order.
    pub fn children<'a>(&'a self, dir: &'a [u8]) -> impl Iterator<Item = &'a [u8]> + 'a {
        let depth = dir.split(|&b| b == b'/').count() + usize::from(!dir.is_empty());
        subtree(&self.nodes, dir, false)
            .map(|(p, _)| p.as_slice())
            .filter(move |p| p.split(|&b| b == b'/').count() == depth)
    }

    /// Puts `stage` on top.
    pub fn apply(&mut self, stage: &Stage) {
        // In order, a directory before what's in it.
        for (path, node) in &stage.nodes {
            match &node.kind {
                Kind::Whiteout => self.remove(path),
                Kind::Dir { opaque } => {
                    let old = self.nodes.get(path);
                    let meta = match (&node.meta, old) {
                        (Some(meta), _) => meta.clone(),
                        (None, Some(Below::Dir(meta))) => meta.clone(),
                        (None, _) => Meta::dir(),
                    };
                    if *opaque || !matches!(old, Some(Below::Dir(_))) {
                        self.remove(path);
                    }
                    self.nodes.insert(path.clone(), Below::Dir(meta));
                }
                Kind::Hardlink { .. } | Kind::Other => {
                    self.remove(path);
                    self.nodes.insert(path.clone(), Below::Other);
                }
            }
        }
    }

    fn remove(&mut self, path: &[u8]) {
        let doomed: Vec<Vec<u8>> = subtree(&self.nodes, path, true)
            .map(|(p, _)| p.clone())
            .collect();
        for path in doomed {
            self.nodes.remove(&path);
        }
    }
}

/// What the stage puts above `view` as whiteouts: what its layers delete that's there, and what's
/// in its opaque directories that it doesn't have itself. (The tar mke2fs reads can't carry the
/// xattr that makes a directory opaque to overlayfs; a whiteout for each thing in it does as well.)
pub fn whiteouts(stage: &Stage, view: &View) -> Vec<Vec<u8>> {
    let mut found = Vec::new();
    for (path, node) in &stage.nodes {
        match node.kind {
            Kind::Whiteout if view.get(path).is_some() => found.push(path.clone()),
            Kind::Dir { opaque: true } => hide_below(stage, view, path, &mut found),
            _ => {}
        }
    }
    found.sort();
    found
}

fn hide_below(stage: &Stage, view: &View, dir: &[u8], found: &mut Vec<Vec<u8>>) {
    if !matches!(view.get(dir), Some(Below::Dir(_))) {
        return;
    }
    for child in view.children(dir) {
        match stage.nodes.get(child) {
            None => found.push(child.to_vec()),
            // An opaque one is seen to on its own.
            Some(Node {
                kind: Kind::Dir { opaque: false },
                ..
            }) => hide_below(stage, view, child, found),
            Some(_) => {}
        }
    }
}

/// `path` normalized: no leading `/`, no `.` or empty components. A `..` is refused.
pub fn normalize(path: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(path.len());
    for component in path.split(|&b| b == b'/') {
        match component {
            b"" | b"." => {}
            b".." => {
                return Err(other(format!(
                    "{}: a path in a layer with ..",
                    String::from_utf8_lossy(path)
                )));
            }
            c => {
                if !out.is_empty() {
                    out.push(b'/');
                }
                out.extend_from_slice(c);
            }
        }
    }
    Ok(out)
}

/// The directory `path` is in, and its name in it; None for the root.
pub fn split(path: &[u8]) -> Option<(&[u8], &[u8])> {
    if path.is_empty() {
        return None;
    }
    Some(match path.iter().rposition(|&b| b == b'/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => (&[], path),
    })
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if dir.is_empty() {
        return name.to_vec();
    }
    [dir, b"/", name].concat()
}

/// The lengths of the root, then each directory down to `path`, and `path` itself.
fn prefixes(path: &[u8]) -> impl Iterator<Item = usize> + '_ {
    let slashes = path
        .iter()
        .enumerate()
        .filter(|&(_, &b)| b == b'/')
        .map(|(i, _)| i);
    let whole = (!path.is_empty()).then_some(path.len());
    std::iter::once(0).chain(slashes).chain(whole)
}

/// What's in `path` in `map`, and `path` itself if `inclusive`. What's in a directory sorts after
/// it but not together: "a/b" comes after "a-b". It's all of "a/" to "a0", '0' following '/'.
fn subtree<'a, V>(
    map: &'a BTreeMap<Vec<u8>, V>,
    path: &[u8],
    inclusive: bool,
) -> Box<dyn Iterator<Item = (&'a Vec<u8>, &'a V)> + 'a> {
    let itself = map.get_key_value(path).filter(|_| inclusive).into_iter();
    if path.is_empty() {
        return Box::new(itself.chain(map.range::<[u8], _>((
            std::ops::Bound::Excluded(&[][..]),
            std::ops::Bound::Unbounded,
        ))));
    }
    let from = [path, b"/"].concat();
    let to = [path, b"0"].concat();
    Box::new(itself.chain(map.range(from..to)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(kind: u8) -> Meta {
        Meta {
            kind,
            ..Meta::dir()
        }
    }

    fn link(target: &str) -> Meta {
        Meta {
            link: target.into(),
            ..meta(b'1')
        }
    }

    /// A stage of `layers`, each its entries: a trailing / for a directory, `name -> target` for a
    /// hard link, a file otherwise.
    fn stage(layers: &[&[&str]]) -> Stage {
        let mut stage = Stage::default();
        for (l, entries) in layers.iter().enumerate() {
            for (i, entry) in entries.iter().enumerate() {
                let (path, meta) = if let Some(dir) = entry.strip_suffix('/') {
                    (dir, meta(b'5'))
                } else if let Some((path, target)) = entry.split_once(" -> ") {
                    (path, link(target))
                } else {
                    (*entry, meta(b'0'))
                };
                stage
                    .add((l, i), &normalize(path.as_bytes()).unwrap(), meta)
                    .unwrap();
            }
        }
        stage
    }

    /// The stage as lines: each path, `/` after a directory and `/*` after an opaque one, `-`
    /// before a whiteout.
    fn lines(stage: &Stage) -> Vec<String> {
        stage
            .nodes
            .iter()
            .filter(|(p, _)| !p.is_empty())
            .map(|(p, n)| {
                let p = String::from_utf8_lossy(p);
                match n.kind {
                    Kind::Dir { opaque: false } => format!("{p}/"),
                    Kind::Dir { opaque: true } => format!("{p}/*"),
                    Kind::Whiteout => format!("-{p}"),
                    _ => p.into_owned(),
                }
            })
            .collect()
    }

    fn view(stages: &[Stage]) -> View {
        let mut view = View::default();
        for s in stages {
            view.apply(s);
        }
        view
    }

    fn paths(found: Vec<Vec<u8>>) -> Vec<String> {
        found
            .into_iter()
            .map(|p| String::from_utf8(p).unwrap())
            .collect()
    }

    #[test]
    fn later_layers_replace_and_merge() {
        let s = stage(&[
            &["etc/", "etc/a", "etc/b", "bin/", "bin/sh"],
            &["etc/", "etc/a", "bin"],
        ]);
        assert_eq!(lines(&s), ["bin", "etc/", "etc/a", "etc/b"]);
        assert_eq!(s.nodes[&b"etc/a"[..]].at, Some((1, 1)));
        assert_eq!(s.nodes[&b"etc"[..]].at, Some((1, 0)));
    }

    #[test]
    fn whiteouts_delete_earlier_layers_and_stay_for_the_stages_below() {
        let s = stage(&[
            &["etc/", "etc/a", "etc/d/", "etc/d/x"],
            &["etc/.wh.a", "etc/.wh.d", "etc/.wh.gone"],
        ]);
        assert_eq!(lines(&s), ["etc/", "-etc/a", "-etc/d", "-etc/gone"]);
    }

    #[test]
    fn a_whiteout_spares_its_own_layer() {
        let s = stage(&[
            &["d/", "d/old"],
            &["d/new", "d/.wh.new", ".wh.d", "d/newer"],
        ]);
        assert_eq!(lines(&s), ["d/*", "d/new", "d/newer"]);
    }

    #[test]
    fn a_directory_made_again_after_a_whiteout_is_opaque() {
        let s = stage(&[&["a/", "a/x"], &[".wh.a"], &["a/", "a/y"]]);
        assert_eq!(lines(&s), ["a/*", "a/y"]);
        // Implicitly, as the directory of something new, too.
        let s = stage(&[&[".wh.b"], &["b/z"]]);
        assert_eq!(lines(&s), ["b/*", "b/z"]);
    }

    #[test]
    fn a_directory_replacing_a_file_is_opaque_and_a_file_replacing_one_drops_its_contents() {
        let s = stage(&[&["f", "d/", "d/x"], &["f/", "d"]]);
        assert_eq!(lines(&s), ["d", "f/*"]);
    }

    #[test]
    fn opaque_markers_hide_earlier_layers() {
        let s = stage(&[
            &["d/", "d/a", "d/s/", "d/s/b"],
            &["d/", "d/.wh..wh..opq", "d/c"],
        ]);
        assert_eq!(lines(&s), ["d/*", "d/c"]);
    }

    #[test]
    fn hard_links_remember_what_they_link_to() {
        let s = stage(&[&["a", "b -> a", "c -> ./missing"], &["a"]]);
        assert_eq!(
            s.nodes[&b"b"[..]].kind,
            Kind::Hardlink {
                target: b"a".to_vec(),
                of: Some((0, 0))
            }
        );
        assert_eq!(s.nodes[&b"a"[..]].at, Some((1, 0)));
        assert!(matches!(
            s.nodes[&b"c"[..]].kind,
            Kind::Hardlink { of: None, .. }
        ));
    }

    #[test]
    fn the_root_entry_describes_the_root() {
        let s = stage(&[&["./", "x"]]);
        assert_eq!(s.nodes[&b""[..]].at, Some((0, 0)));
    }

    #[test]
    fn paths_are_normalized_and_dot_dot_refused() {
        assert_eq!(normalize(b"./a//b/./c/").unwrap(), b"a/b/c");
        assert_eq!(normalize(b"/").unwrap(), b"");
        assert!(normalize(b"a/../b").is_err());
    }

    #[test]
    fn children_are_only_whats_directly_in() {
        let v = view(&[stage(&[&["a/", "a/b", "a/c/", "a/c/d", "a-b", "a0"]])]);
        let kids: Vec<_> = v.children(b"a").collect();
        assert_eq!(kids, [&b"a/b"[..], b"a/c"]);
        let top: Vec<_> = v.children(b"").collect();
        assert_eq!(top, [&b"a"[..], b"a-b", b"a0"]);
    }

    #[test]
    fn whiteouts_are_kept_only_for_what_is_below() {
        let below = view(&[stage(&[&["etc/", "etc/a"]])]);
        let s = stage(&[&["etc/.wh.a", "etc/.wh.never"]]);
        assert_eq!(paths(whiteouts(&s, &below)), ["etc/a"]);
    }

    #[test]
    fn opaque_directories_white_out_what_is_below_that_they_lack() {
        let below = view(&[stage(&[&[
            "d/", "d/a", "d/b", "d/s/", "d/s/x", "d/s/y", "d/t/", "d/t/z",
        ]])]);
        let s = stage(&[&[".wh.d"], &["d/", "d/b", "d/s/", "d/s/y"]]);
        assert_eq!(paths(whiteouts(&s, &below)), ["d/a", "d/s/x", "d/t"]);
    }

    #[test]
    fn the_view_is_the_stages_stacked() {
        let v = view(&[
            stage(&[&["a/", "a/x", "b/", "b/y", "c"]]),
            stage(&[&[".wh.a", "b/", "b/.wh..wh..opq", "b/z", "c/", "c/w"]]),
        ]);
        let all: Vec<_> = v
            .nodes
            .keys()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        assert_eq!(all, ["", "b", "b/z", "c", "c/w"]);
    }
}
