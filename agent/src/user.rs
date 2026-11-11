//! Users and groups, from the image's /etc/passwd and /etc/group: the guest has no NSS to ask.

use std::fs;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: String,
    /// Every group the user is in: its own, then those that list it as a member.
    pub groups: Vec<u32>,
}

/// The user called `name`, looked up under `root` (/ in the guest).
pub fn lookup(root: &Path, name: &str) -> Result<User, String> {
    let passwd = fs::read_to_string(root.join("etc/passwd"))
        .map_err(|e| format!("reading /etc/passwd: {e}"))?;
    let entry = passwd
        .lines()
        .map(|line| line.split(':').collect::<Vec<_>>())
        .find(|fields| fields.len() >= 7 && fields[0] == name)
        .ok_or_else(|| format!("no such user: {name}"))?;
    let number = |field: &str| {
        field
            .parse::<u32>()
            .map_err(|_| format!("/etc/passwd has a bad entry for {name}"))
    };
    let (uid, gid) = (number(entry[2])?, number(entry[3])?);
    let mut groups = vec![gid];
    // No /etc/group: just the user's own group.
    for line in fs::read_to_string(root.join("etc/group"))
        .unwrap_or_default()
        .lines()
    {
        let fields: Vec<_> = line.split(':').collect();
        if fields.len() >= 4
            && fields[3].split(',').any(|member| member == name)
            && let Ok(group) = fields[2].parse::<u32>()
            && !groups.contains(&group)
        {
            groups.push(group);
        }
    }
    Ok(User {
        name: name.to_string(),
        uid,
        gid,
        home: entry[5].to_string(),
        groups,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(passwd: &str, group: Option<&str>) -> tempdir::Dir {
        let dir = tempdir::Dir::new();
        fs::create_dir(dir.path().join("etc")).unwrap();
        fs::write(dir.path().join("etc/passwd"), passwd).unwrap();
        if let Some(group) = group {
            fs::write(dir.path().join("etc/group"), group).unwrap();
        }
        dir
    }

    const PASSWD: &str =
        "root:x:0:0:root:/root:/bin/bash\ngenie:x:1000:1000::/home/genie:/bin/bash\n";

    #[test]
    fn finds_a_user_and_its_groups() {
        let dir = root(
            PASSWD,
            Some(
                "root:x:0:\ngenie:x:1000:\nsudo:x:27:genie\nusers:x:100:other,genie\nnope:x:5:other\n",
            ),
        );
        let user = lookup(dir.path(), "genie").unwrap();
        assert_eq!(
            user,
            User {
                name: "genie".into(),
                uid: 1000,
                gid: 1000,
                home: "/home/genie".into(),
                groups: vec![1000, 27, 100],
            }
        );
    }

    #[test]
    fn without_etc_group_a_user_has_its_own_group() {
        let dir = root(PASSWD, None);
        assert_eq!(lookup(dir.path(), "root").unwrap().groups, vec![0]);
    }

    #[test]
    fn an_unknown_user_is_an_error() {
        let dir = root(PASSWD, None);
        assert_eq!(
            lookup(dir.path(), "nobody2").unwrap_err(),
            "no such user: nobody2"
        );
    }

    /// A directory removed when dropped: enough for these tests, without a crate for it.
    mod tempdir {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU32, Ordering};

        pub struct Dir(PathBuf);

        impl Dir {
            pub fn new() -> Self {
                static N: AtomicU32 = AtomicU32::new(0);
                let path = std::env::temp_dir().join(format!(
                    "bottle-agent-test-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&path).unwrap();
                Dir(path)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
