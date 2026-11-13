//! `bottle-cli image tag|rm`, `images` and `gc` end to end, on a store of stages made here: none
//! of them needs a VM (`image build` does; see bottle-disk's tests and the e2e ones).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Store {
    dir: PathBuf,
    home: PathBuf,
}

impl Store {
    /// A store of stages `base` and `tools` on it, and `old`: none tagged, no machines.
    fn new(name: &str) -> Store {
        let root =
            std::env::temp_dir().join(format!("bottle-images-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        // Its home's: the store is $BOTTLE_HOME/stages.
        let (dir, home) = (root.join("stages"), root.clone());
        fs::create_dir_all(&dir).unwrap();
        for (stage, lowers) in [("base", vec![]), ("tools", vec!["base"]), ("old", vec![])] {
            fs::write(dir.join(format!("{stage}.ext4")), "").unwrap();
            let json = serde_json::json!({"lowers": lowers, "env": {}, "entrypoint": null, "cmd": null,
                                          "labels": {"made": stage}});
            fs::write(dir.join(format!("{stage}.json")), json.to_string()).unwrap();
        }
        Store { dir, home }
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bottle-cli"))
            .args(args)
            .env("BOTTLE_HOME", &self.home)
            .env_remove("BOTTLE_MKE2FS")
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.cli(args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn stages(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.dir)
            .unwrap()
            .filter_map(|e| e.unwrap().file_name().into_string().ok())
            .filter_map(|n| n.strip_suffix(".ext4").map(String::from))
            .collect();
        names.sort();
        names
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.dir.parent().unwrap());
    }
}

fn disk(store: &Store, stage: &str) -> String {
    store
        .dir
        .join(format!("{stage}.ext4"))
        .display()
        .to_string()
}

#[test]
fn a_tag_names_an_image_its_listed_by_and_gc_keeps() {
    let store = Store::new("tags");
    let printed = store.ok(&["image", "tag", "tools", "localhost/bottle/base:with-tools"]);
    assert_eq!(
        printed.lines().collect::<Vec<_>>(),
        [disk(&store, "base"), disk(&store, "tools")]
    );
    let listed: serde_json::Value = serde_json::from_str(&store.ok(&["images", "--json"])).unwrap();
    assert_eq!(
        listed,
        serde_json::json!([{"tag": "localhost/bottle/base:with-tools", "name": "tools",
                            "disks": [disk(&store, "base"), disk(&store, "tools")],
                            "labels": {"made": "tools"}}])
    );
    // A tag names an image as a stage's name does.
    store.ok(&["image", "tag", "localhost/bottle/base:with-tools", "again"]);
    assert_eq!(store.ok(&["images"]).lines().count(), 2);

    let deleted = store.ok(&["gc"]);
    assert_eq!(deleted.trim(), "deleted old"); // untagged, unused; tools' lower stays with it
    assert_eq!(store.stages(), ["base", "tools"]);

    store.ok(&["image", "rm", "again"]);
    store.ok(&["image", "rm", "localhost/bottle/base:with-tools"]);
    let out = store.cli(&["image", "rm", "localhost/bottle/base:with-tools"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no image tagged"));
    store.ok(&["gc"]);
    assert!(store.stages().is_empty());
}

#[test]
fn what_isnt_there_cant_be_tagged() {
    let store = Store::new("missing");
    let out = store.cli(&["image", "tag", "nope", "t"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no image nope"));
    assert!(!Path::new(&store.dir.join("tags.json")).exists());
}
