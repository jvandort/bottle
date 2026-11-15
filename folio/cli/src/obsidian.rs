//! Opening notes -- a thread's, or the broker's -- in Obsidian, on a Mac.
//!
//! Obsidian has no documented way to make a folder a vault: its URIs only open vaults it already
//! knows, and its CLI can't add one. So folio does what "Open folder as vault" does itself, the
//! undocumented way:
//!
//! - **The vault's `.obsidian/`**, Obsidian's settings for it, is made if it's missing, so Obsidian
//!   takes the folder for a vault it has opened before rather than a new one (the vault's .gitignore
//!   keeps it out of git).
//! - **Obsidian's list of vaults**, obsidian.json, gets an entry for it: `{"vaults": {ID: {"path":
//!   ..., "ts": ...}}}`, an ID of 16 hex characters and the time in milliseconds, as Obsidian writes
//!   them. Obsidian reads the list when it starts, and may write it over while it runs, so a running
//!   Obsidian is quit first (asking, on a terminal) and opened again on the vault.
//!
//! A vault Obsidian knows is opened by its ID (`obsidian://open?vault=`; `path=` is for a file
//! inside a vault, and its root isn't one). If obsidian.json isn't the shape described, nothing is
//! written, and Obsidian's vault manager is opened instead, with the path on the clipboard for "Open
//! folder as vault".

use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

const QUIT_TIMEOUT: Duration = Duration::from_secs(10);

fn config() -> PathBuf {
    match std::env::var_os("FOLIO_OBSIDIAN_CONFIG").filter(|c| !c.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join("Library/Application Support/obsidian/obsidian.json"),
    }
}

/// macOS's `open`, `pgrep` or `osascript`; FOLIO_OPEN and so on stand in for them in tests.
fn command(name: &str) -> String {
    std::env::var(format!("FOLIO_{}", name.to_uppercase()))
        .ok()
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| name.into())
}

fn quiet(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn on_path(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

pub fn installed() -> bool {
    let opener = command("open");
    on_path(&opener) && quiet(&opener, &["-Ra", "Obsidian"])
}

/// obsidian.json's vaults, by ID; empty if there's no obsidian.json; None if it isn't the shape
/// folio knows.
fn vaults() -> Option<Map<String, Value>> {
    let text = match std::fs::read_to_string(config()) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Map::new()),
        Err(_) => return None,
    };
    let config: Value = serde_json::from_str(&text).ok()?;
    let vaults = config.get("vaults")?.as_object()?;
    vaults
        .values()
        .all(|v| v.get("path").is_some_and(Value::is_string))
        .then(|| vaults.clone())
}

fn resolve(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// The ID Obsidian knows the vault at `path` by; None if it doesn't know one there.
fn vault_id(path: &Path) -> Option<String> {
    let path = resolve(path);
    vaults()?
        .into_iter()
        .find(|(_, vault)| resolve(Path::new(vault["path"].as_str().unwrap_or_default())) == path)
        .map(|(id, _)| id)
}

fn running() -> bool {
    quiet(&command("pgrep"), &["-x", "Obsidian"])
}

/// Quit Obsidian, as its menu would, and wait for it to go; whether it has.
fn quit() -> bool {
    quiet(&command("osascript"), &["-e", "quit app \"Obsidian\""]);
    let deadline = Instant::now() + QUIT_TIMEOUT;
    while running() {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    true
}

/// Add the vault at `path` to obsidian.json, keeping everything else in it; its new ID.
fn register(path: &Path) -> Result<String, String> {
    let config_path = config();
    let mut config: Value = match std::fs::read_to_string(&config_path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| e.to_string())?,
        Err(_) => json!({"vaults": {}}),
    };
    let mut bytes = [0u8; 8];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| e.to_string())?;
    let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    config["vaults"][&id] = json!({"path": resolve(path).display().to_string(), "ts": ts});
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let name = config_path.file_name().unwrap_or_default().to_string_lossy();
    let staging = config_path.with_file_name(format!(".{name}.folio"));
    std::fs::write(&staging, crate::bottle_cli::python_dumps(&config)).map_err(|e| e.to_string())?;
    std::fs::rename(&staging, &config_path).map_err(|e| e.to_string())?;
    Ok(id)
}

fn confirm(question: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        return false;
    }
    print!("{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) | Err(_) => {
            println!();
            false
        }
        Ok(_) => matches!(line.trim().to_lowercase().as_str(), "y" | "yes"),
    }
}

/// `s`, quoted for a URL's query as Python's urllib.parse.quote has it.
fn url_quote(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"_.-~/".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Open the vault at `path` in Obsidian, making it one Obsidian knows first; what to tell the user.
pub fn open_vault(path: &Path) -> Result<String, String> {
    std::fs::create_dir_all(path.join(".obsidian")).map_err(|e| e.to_string())?;
    let mut known = vault_id(path);
    if known.is_none() && vaults().is_some() {
        if running() {
            if !confirm("Obsidian has to restart to add the notes as a vault. Quit and reopen it?") {
                return Ok("Obsidian is running, and has to restart to add the notes as a vault: quit it, and run \
                           the command again."
                    .into());
            }
            if !quit() {
                return Ok("Obsidian didn't quit; quit it, and run the command again.".into());
            }
        }
        known = Some(register(path)?);
    }
    if let Some(id) = known {
        let _ = Command::new(command("open")).arg(format!("obsidian://open?vault={}", url_quote(&id))).status();
        return Ok(format!("Opened {} in Obsidian.", path.display()));
    }
    let copied = on_path("pbcopy")
        && Command::new("pbcopy")
            .stdin(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                child.stdin.take().unwrap().write_all(path.display().to_string().as_bytes())?;
                child.wait()
            })
            .is_ok_and(|s| s.success());
    let _ = Command::new(command("open")).arg("obsidian://choose-vault").status();
    Ok(format!(
        "Obsidian's list of vaults isn't what folio expects, so it's left alone, and Obsidian's vault manager is \
         open: choose \"Open folder as vault\" and open\n\n    {}\n\n{}in the folder picker, ⌘⇧G and {} (~/.folio \
         is hidden).",
        path.display(),
        if copied { "The path is on your clipboard: " } else { "" },
        if copied { "paste it" } else { "type it" }
    ))
}
