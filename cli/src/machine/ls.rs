//! `ls`: every machine, and what it was made with.

use super::*;

pub(super) fn ls(args: &[String]) -> io::Result<()> {
    let json = args.iter().any(|a| a == "--json");
    let mut found = Vec::new();
    if let Ok(entries) = fs::read_dir(machines()) {
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(String::from) else {
                continue;
            };
            if let Ok(machine) = Machine::new(&name)
                && let Some(state) = machine.state()
            {
                found.push((name, state, machine.config().ok()));
            }
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    let rows: Vec<listing::Machine> = found
        .into_iter()
        .map(|(name, state, config)| listing::Machine {
            name,
            state,
            made: config.map(|config| listing::Made {
                cpus: config.cpus,
                mem_mib: config.mem_mib,
                storage_gib: config.storage_gib,
                image: image_name(&config.image),
                labels: config.labels,
            }),
        })
        .collect();
    if json {
        println!(
            "{}",
            serde_json::to_string(&rows).map_err(io::Error::other)?
        );
        return Ok(());
    }
    let mut table = vec![[
        "NAME".to_string(),
        "STATE".into(),
        "CPUS".into(),
        "MEMORY".into(),
        "IMAGE".into(),
        "LABELS".into(),
    ]];
    for row in &rows {
        let (cpus, memory, image, labels) = match &row.made {
            Some(made) => (
                made.cpus.to_string(),
                format!("{}M", made.mem_mib),
                made.image.clone(),
                made.labels
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            None => Default::default(),
        };
        table.push([
            row.name.clone(),
            row.state.to_string(),
            cpus,
            memory,
            image,
            labels,
        ]);
    }
    let widths: Vec<usize> = (0..6)
        .map(|i| table.iter().map(|r| r[i].len()).max().unwrap_or(0))
        .collect();
    for row in table {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:width$}"))
            .collect();
        println!("{}", line.join("  ").trim_end());
    }
    Ok(())
}

/// What a machine's image is called: a tag of its top stage in its store, else the stage's name.
pub(super) fn image_name(disks: &[PathBuf]) -> String {
    let Some(top) = disks.last() else {
        return String::new();
    };
    let stage = top
        .file_stem()
        .map_or(String::new(), |s| s.to_string_lossy().into_owned());
    let store = Store {
        dir: top.parent().map(Path::to_path_buf).unwrap_or_default(),
        mke2fs: PathBuf::new(),
    };
    store
        .tags()
        .ok()
        .and_then(|tags| {
            tags.into_iter()
                .find(|(_, name)| *name == stage)
                .map(|(tag, _)| tag)
        })
        .unwrap_or(stage)
}
