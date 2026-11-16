//! What builds a bottle's image: Debian, then the base feature (the bottle contract), then a
//! bottle's features, each a stage bottle-cli builds from a recipe (bottle_disk::build::Recipe):
//! a step for what the features install with apt, all at once, then each feature's install.sh, run
//! in its own directory, the features directory mounted read-only in the build, then its
//! containerEnv. Every image records what it was built from (its inputs, a hash), so one whose
//! inputs have changed is rebuilt when next needed; builds.rs builds them.
//!
//! What's kept of what bottles download (the HTTP cache bottle-cli keeps): the hosts features name,
//! and every apt repository an image has, its packages, the .deb files under its URI.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::features::{self, BASE, Feature, Repository};

/// Debian 13 by name: a build takes trixie's latest point release and security fixes.
pub const DEBIAN: &str = "debian:trixie-slim";
/// What a bottle runs when it starts: the base feature's entrypoint, under tini, and sshd.
pub const ENTRYPOINT: [&str; 4] = [
    "/usr/bin/tini",
    "-s",
    "--",
    "/usr/local/bin/bottle-entrypoint",
];
pub const CMD: [&str; 3] = ["/usr/sbin/sshd", "-D", "-e"];
/// What bottle's images are called.
pub const REPOSITORY: &str = "localhost/bottle";
/// Where a build's directory is in its VM.
pub const MOUNT: &str = "/run/bottle-build";
/// A step with this in its environment gets the certificate of the caching CA builds share.
pub const BUILD_CA: &str = "BOTTLE_BUILD_CA";
/// The label an image with features records them in.
pub const FEATURES_LABEL: &str = "bottle.features";
/// Who the container's user is, as a feature's install.sh is told.
const REMOTE_USER: &str = "genie";
/// Where an apt repository is asked, by its URI's scheme.
const HTTP_PORT: u16 = 80;
const HTTPS_PORT: u16 = 443;

type Result<T> = std::result::Result<T, String>;

pub fn tag(name: &str) -> String {
    format!("{REPOSITORY}/{name}:latest")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A hash of every file under `path`: names, contents and whether each is executable, in order.
pub fn tree_hash(path: &Path) -> Result<String> {
    fn walk(dir: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                walk(&path, files)?;
            } else if path.is_file() {
                files.push(path);
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    if path.is_dir() {
        walk(path, &mut files).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    files.sort();
    let mut digest = Sha256::new();
    for file in &files {
        let mode = fs::metadata(file)
            .map_err(|e| e.to_string())?
            .permissions()
            .mode()
            & 0o111;
        let name = file.strip_prefix(path).unwrap().to_string_lossy();
        digest.update(format!("{name}\0{mode:o}\0").as_bytes());
        digest.update(Sha256::digest(fs::read(file).map_err(|e| e.to_string())?));
    }
    Ok(hex(&digest.finalize()))
}

/// How to run `script`, in its directory: by its #! line, so it needn't be executable.
fn interpreter(script: &Path) -> Result<Vec<String>> {
    let text = fs::read_to_string(script).map_err(|e| format!("{}: {e}", script.display()))?;
    let first = text.split('\n').next().unwrap_or_default();
    let mut run: Vec<String> = match first.strip_prefix("#!") {
        Some(line) => line.split_whitespace().map(String::from).collect(),
        None => vec!["/bin/sh".into()],
    };
    run.push(format!(
        "./{}",
        script.file_name().unwrap().to_string_lossy()
    ));
    Ok(run)
}

/// The tag of the base image with `features`: the same features and options always get the same
/// tag, a feature with options set its id and a short hash of its spec.
pub fn image_tag(features: &[Feature]) -> String {
    if features.is_empty() {
        return tag(BASE);
    }
    let mut parts: Vec<String> = features
        .iter()
        .map(|f| {
            if f.overrides.is_empty() {
                f.id.clone()
            } else {
                format!(
                    "{}-{}",
                    f.id,
                    &hex(&Sha256::digest(f.spec().as_bytes()))[..8]
                )
            }
        })
        .collect();
    parts.sort();
    format!("{REPOSITORY}/{BASE}:with-{}", parts.join("."))
}

/// What the base image is built from: Debian's reference, what it runs, and the base feature's
/// directory.
pub fn base_inputs() -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(json!([DEBIAN, ENTRYPOINT, CMD]).to_string().as_bytes());
    digest.update(tree_hash(&features::dir().join(BASE))?.as_bytes());
    Ok(hex(&digest.finalize()))
}

/// What the base image with `features` is built from: base's inputs, and each feature's directory
/// and options.
pub fn inputs_hash(features: &[Feature]) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(base_inputs()?.as_bytes());
    for f in features {
        digest.update(format!("\0{}\0{}", f.spec(), tree_hash(&f.path)?).as_bytes());
    }
    Ok(hex(&digest.finalize()))
}

fn step_env() -> serde_json::Map<String, Value> {
    let mut env = serde_json::Map::new();
    env.insert("HOME".into(), "/root".into());
    env.insert("DEBIAN_FRONTEND".into(), "noninteractive".into());
    env
}

/// What installs `features`, in order: what they install with apt, together; then each one's
/// install.sh, in its own directory; then its containerEnv, mirrored to /etc/environment, which is
/// all SSH sessions see.
fn steps(features: &[Feature]) -> Result<Vec<Value>> {
    let mut steps = Vec::new();
    let repositories: Vec<(String, &Repository)> = features
        .iter()
        .flat_map(|f| {
            f.apt
                .repositories
                .iter()
                .map(move |r| (format!("{MOUNT}/{}/{}", f.id, r.key), r))
        })
        .collect();
    let mut packages: Vec<String> = Vec::new();
    for f in features {
        for p in f.apt_packages()? {
            if !packages.contains(&p) {
                packages.push(p);
            }
        }
    }
    if !repositories.is_empty() || !packages.is_empty() {
        let mut env = step_env();
        env.insert(BUILD_CA.into(), "".into());
        steps.push(
            json!({"run": ["/bin/sh", "-c", install_script(&repositories, &packages)], "env": env}),
        );
    }
    for f in features {
        let mut env = step_env();
        env.insert("_REMOTE_USER".into(), REMOTE_USER.into());
        env.insert(
            "_REMOTE_USER_HOME".into(),
            format!("/home/{REMOTE_USER}").into(),
        );
        env.insert("_CONTAINER_USER".into(), REMOTE_USER.into());
        env.insert(
            "_CONTAINER_USER_HOME".into(),
            format!("/home/{REMOTE_USER}").into(),
        );
        for (k, v) in f.option_env() {
            env.insert(k, v.into());
        }
        steps.push(json!({
            "run": interpreter(&f.path.join("install.sh"))?,
            "env": env,
            "cwd": format!("{MOUNT}/{}", f.id),
        }));
        if !f.container_env.is_empty() {
            steps.push(json!({"env": f.container_env}));
            let mirror: Vec<String> = f
                .container_env
                .keys()
                .map(|k| {
                    format!("sed -i '/^{k}=/d' /etc/environment && printf '%s\\n' \"{k}=${k}\" >> /etc/environment")
                })
                .collect();
            steps.push(json!({"run": ["sh", "-c", mirror.join(" && ")]}));
        }
    }
    Ok(steps)
}

/// What builds the base image: its tag, inputs and recipe.
pub fn base() -> Result<Value> {
    let base = features::load(BASE)?;
    Ok(json!({
        "tag": tag(BASE),
        "inputs": base_inputs()?,
        "recipe": {
            "pull": DEBIAN, "entrypoint": ENTRYPOINT, "cmd": CMD, "labels": {},
            "steps": steps(std::slice::from_ref(&base))?,
        },
    }))
}

/// What builds the base image with `features`: its tag, inputs and recipe.
pub fn image(features: &[Feature]) -> Result<Value> {
    let installed: Vec<String> = features.iter().map(Feature::spec).collect();
    let mut specs = installed.clone();
    specs.sort();
    Ok(json!({
        "tag": image_tag(features),
        "inputs": inputs_hash(features)?,
        "recipe": {
            "from": tag(BASE),
            "steps": steps(features)?,
            "features": installed,
            "labels": {FEATURES_LABEL: specs.join(" ")},
        },
    }))
}

// --- apt ------------------------------------------------------------------------

/// Debian's repositories, as debian:trixie-slim's sources have them.
fn debian() -> Vec<Repository> {
    [
        "http://deb.debian.org/debian",
        "http://deb.debian.org/debian-security",
    ]
    .map(|uri| Repository {
        uri: uri.into(),
        suite: String::new(),
        components: vec![],
        key: String::new(),
    })
    .to_vec()
}

/// A URI's scheme, host (lowercase, without a port) and path.
fn split(uri: &str) -> (&str, String, &str) {
    let (scheme, rest) = uri.split_once("://").unwrap_or(("", uri));
    let (authority, path) = rest
        .find('/')
        .map_or((rest, ""), |i| (&rest[..i], &rest[i..]));
    let host = authority.rsplit('@').next().unwrap_or_default();
    let host = host.split(':').next().unwrap_or_default().to_lowercase();
    (scheme, host, path)
}

/// The shell that installs `repositories` (each with where its key is in the build), then
/// `packages`: each repository's key in /etc/apt/keyrings and its sources signed by it; a build's
/// https repositories answered from bottle-cli's cache, behind the CA in $BOTTLE_BUILD_CA, which
/// apt trusts for their hosts alone, from a file in /run that doesn't stay in the image.
fn install_script(repositories: &[(String, &Repository)], packages: &[String]) -> String {
    let mut lines = vec!["set -eu".to_string()];
    for (at, r) in repositories {
        let file = Path::new(at).file_name().unwrap().to_string_lossy();
        let keyring = format!("/etc/apt/keyrings/{file}");
        let source = format!(
            "deb [signed-by={keyring}] {} {} {}",
            r.uri,
            r.suite,
            r.components.join(" ")
        );
        lines.push(format!(
            "install -D -m 0644 {} {}",
            crate::shell::quote(at),
            crate::shell::quote(&keyring)
        ));
        lines.push(format!(
            "echo {} >> /etc/apt/sources.list.d/{}.list",
            crate::shell::quote(&source),
            list_name(r)
        ));
    }
    let mut https: Vec<String> = repositories
        .iter()
        .filter(|(_, r)| split(&r.uri).0 == "https")
        .map(|(_, r)| split(&r.uri).1)
        .collect();
    https.sort();
    https.dedup();
    lines.push("options=\"\"".into());
    lines.push("if [ -n \"${BOTTLE_BUILD_CA:-}\" ]; then".into());
    lines.push("  printf '%s\\n' \"$BOTTLE_BUILD_CA\" > /run/bottle-build-ca.pem".into());
    for host in https {
        lines.push(format!(
            "  options=\"$options -o Acquire::https::{host}::CaInfo=/run/bottle-build-ca.pem\""
        ));
    }
    lines.push("fi".into());
    lines.push("# shellcheck disable=SC2086 # word splitting is intended".into());
    lines.push("apt-get $options update".into());
    let quoted: Vec<String> = packages.iter().map(|p| crate::shell::quote(p)).collect();
    lines.push(format!(
        "apt-get $options install -y --no-install-recommends {}",
        quoted.join(" ")
    ));
    lines.push("rm -rf /var/lib/apt/lists/* /run/bottle-build-ca.pem".into());
    lines.join("\n") + "\n"
}

/// A repository's sources' file name: its host and path, as a name.
fn list_name(r: &Repository) -> String {
    let (_, host, path) = split(&r.uri);
    // As re.sub(r"[^A-Za-z0-9.-]+", "-", ...): each run of anything else one dash.
    let mut name = String::new();
    let mut run = false;
    for c in format!("{host}{path}").chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
            name.push(c);
            run = false;
        } else if !run {
            name.push('-');
            run = true;
        }
    }
    name.trim_matches('-').to_string()
}

// --- the cache --------------------------------------------------------------------

/// How `repositories`' downloads are kept: their packages, the .deb files under each one's URI, on
/// its scheme's port; one entry a host and port, its repositories' paths together.
fn apt_cached(repositories: &[Repository]) -> Vec<Value> {
    let mut merged: Vec<(String, u16, Vec<String>)> = Vec::new();
    for r in repositories {
        let (scheme, host, path) = split(&r.uri);
        let port = if scheme == "https" {
            HTTPS_PORT
        } else {
            HTTP_PORT
        };
        let include = format!("{}/*.deb", path.trim_end_matches('/'));
        match merged.iter_mut().find(|(h, p, _)| *h == host && *p == port) {
            Some((_, _, includes)) => includes.push(include),
            None => merged.push((host, port, vec![include])),
        }
    }
    merged
        .into_iter()
        .map(|(host, port, include)| {
            json!({"host": host, "exclude": [], "include": include, "ports": [port], "immutable": true})
        })
        .collect()
}

/// The hosts a bottle with `features` has its downloads kept for: those its features name, and the
/// apt repositories its image has.
pub fn cached_for(features: &[Feature]) -> Value {
    let mut cached: Vec<Value> = features
        .iter()
        .filter_map(|f| f.cache.as_ref())
        .flat_map(|c| {
            c.hosts.iter().map(|h| {
                json!({"host": h.to_lowercase(), "exclude": c.exclude, "include": [],
                       "ports": [HTTPS_PORT], "immutable": false})
            })
        })
        .collect();
    let mut repositories = debian();
    repositories.extend(features.iter().flat_map(|f| f.apt.repositories.clone()));
    cached.extend(apt_cached(&repositories));
    Value::Array(cached)
}

/// What a build keeps of what it downloads: every apt repository, Debian's and every feature's, so
/// every build's policy is the same, and so is the caching CA bottle-cli answers its https ones
/// with.
pub fn build_cached() -> Result<Value> {
    let mut repositories = debian();
    for id in std::iter::once(BASE.to_string()).chain(features::available()) {
        repositories.extend(features::load(&id)?.apt.repositories);
    }
    Ok(Value::Array(apt_cached(&repositories)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository(uri: &str) -> Repository {
        Repository {
            uri: uri.into(),
            suite: String::new(),
            components: vec![],
            key: String::new(),
        }
    }

    #[test]
    fn every_bottle_keeps_debians_packages_and_a_build_apts_alone() {
        let cached = cached_for(&[]);
        let debian = cached
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["host"] == "deb.debian.org")
            .unwrap();
        let mut include: Vec<&str> = debian["include"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i.as_str().unwrap())
            .collect();
        include.sort();
        assert_eq!(include, ["/debian-security/*.deb", "/debian/*.deb"]);
        assert_eq!(
            (&debian["ports"], &debian["immutable"]),
            (&json!([HTTP_PORT]), &json!(true))
        );
        // Maven's hosts are the jvm feature's to cache in a bottle; in a build only apt trusts the
        // builds' CA.
        let hosts = |cached: &Value| {
            cached
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["host"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        let jvm = features::resolve(&["jvm".to_string()]).unwrap();
        assert!(hosts(&cached_for(&jvm)).contains(&"repo.maven.apache.org".to_string()));
        let built = hosts(&build_cached().unwrap());
        assert!(
            !built.contains(&"repo.maven.apache.org".to_string())
                && built.contains(&"deb.debian.org".to_string())
        );
    }

    #[test]
    fn images_are_named_in_full_and_base_is_the_base_feature_on_debian() {
        assert_eq!(tag("base"), "localhost/bottle/base:latest");
        let recipe = base().unwrap()["recipe"].clone();
        assert_eq!(recipe["pull"], DEBIAN);
        let steps = recipe["steps"].as_array().unwrap();
        assert!(steps[0]["run"][2].as_str().unwrap().contains(
            "install -y --no-install-recommends ca-certificates git openssh-server sudo tini"
        ));
        assert_eq!(steps[1]["run"], json!(["/bin/sh", "./install.sh"]));
        assert_eq!(steps[2]["env"]["LANG"], "C.UTF-8");
    }

    #[test]
    fn a_script_is_run_by_its_hash_bang_line() {
        // A read-only mount can't be made executable.
        let tmp = crate::tempdir::Dir::new();
        let script = tmp.path().join("install.sh");
        for (first, argv) in [
            ("#!/bin/bash -e", &["/bin/bash", "-e"][..]),
            ("#!/bin/sh", &["/bin/sh"]),
            ("set -e", &["/bin/sh"]),
        ] {
            std::fs::write(&script, format!("{first}\n")).unwrap();
            let mut expected: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
            expected.push("./install.sh".into());
            assert_eq!(interpreter(&script).unwrap(), expected);
        }
    }

    #[test]
    fn a_trees_hash_tracks_names_contents_and_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = crate::tempdir::Dir::new();
        let install = tmp.path().join("install.sh");
        std::fs::write(&install, "apt-get install a\n").unwrap();
        let before = tree_hash(tmp.path()).unwrap();
        assert_eq!(tree_hash(tmp.path()).unwrap(), before);
        std::fs::write(&install, "apt-get install b\n").unwrap();
        let edited = tree_hash(tmp.path()).unwrap();
        std::fs::set_permissions(&install, std::fs::Permissions::from_mode(0o755)).unwrap();
        let executable = tree_hash(tmp.path()).unwrap();
        std::fs::write(tmp.path().join("extra"), "").unwrap();
        let more = tree_hash(tmp.path()).unwrap();
        let all: std::collections::BTreeSet<_> =
            [before, edited, executable, more].into_iter().collect();
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn a_repositorys_packages_are_kept_and_only_those_on_its_schemes_port() {
        let kept = apt_cached(&[repository("https://downloads.example/claude/apt/stable/")]);
        assert_eq!(
            kept,
            [
                json!({"host": "downloads.example", "exclude": [], "include": ["/claude/apt/stable/*.deb"],
                    "ports": [443], "immutable": true})
            ]
        );
        assert_eq!(
            apt_cached(&[repository("http://deb.debian.org/debian")])[0]["ports"],
            json!([80])
        );
    }

    #[test]
    fn a_hosts_repositories_are_one_entry() {
        let [debian] = apt_cached(&debian()).try_into().unwrap();
        assert_eq!(
            debian["include"],
            json!(["/debian/*.deb", "/debian-security/*.deb"])
        );
    }

    #[test]
    fn a_repositorys_sources_are_named_for_its_host_and_path() {
        let r = Repository {
            uri: "https://Packages.Example.com/apt/x_y/".into(),
            suite: "stable".into(),
            components: vec!["main".into()],
            key: "k.gpg".into(),
        };
        assert_eq!(list_name(&r), "packages.example.com-apt-x-y");
    }
}
