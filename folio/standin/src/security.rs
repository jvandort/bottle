//! macOS's `security`, as far as folio uses it: generic passwords kept as files in $FAKE_KEYCHAIN,
//! a file a service and account. Every command line is logged in $FAKE_KEYCHAIN/argv, and what
//! `-i` read, in $FAKE_KEYCHAIN/stdin.

use std::fs;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

/// What `security` says of a password it hasn't.
const NOT_FOUND: u8 = 44;

fn append(path: PathBuf, line: &str) {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    writeln!(file, "{line}").unwrap();
}

/// The value after `option` in `args`.
fn option<'a>(args: &'a [String], option: &str) -> &'a str {
    args.windows(2)
        .find(|pair| pair[0] == option)
        .map_or("", |pair| pair[1].as_str())
}

pub fn main(args: Vec<String>) -> ExitCode {
    let store = PathBuf::from(std::env::var_os("FAKE_KEYCHAIN").expect("FAKE_KEYCHAIN"));
    fs::create_dir_all(&store).unwrap();
    append(store.join("argv"), &args.join(" "));
    let item =
        |args: &[String]| store.join(format!("{}@{}", option(args, "-s"), option(args, "-a")));
    match args.first().map(String::as_str) {
        // A command on stdin: add-generic-password -U -s "SERVICE" -a "ACCOUNT" -w "SECRET".
        Some("-i") => {
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line).unwrap();
            let line = line.trim_end_matches('\n');
            append(store.join("stdin"), line);
            let fields: Vec<&str> = line.split('"').collect();
            let field = |n: usize| fields.get(n).copied().unwrap_or_default();
            fs::write(store.join(format!("{}@{}", field(1), field(3))), field(5)).unwrap();
            ExitCode::SUCCESS
        }
        Some("find-generic-password") => match fs::read_to_string(item(&args)) {
            Ok(secret) => {
                println!("{secret}");
                ExitCode::SUCCESS
            }
            Err(_) => ExitCode::from(NOT_FOUND),
        },
        Some("delete-generic-password") => match fs::remove_file(item(&args)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::from(NOT_FOUND),
        },
        _ => {
            eprintln!("security: the stand-in doesn't know {}", args.join(" "));
            ExitCode::from(1)
        }
    }
}
