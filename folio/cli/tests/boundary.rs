//! folio runs bottle's code by running bottle-cli, never by linking it: folio-cli depends on none of
//! bottle's crates. Libraries both use (serde, sha2) are fine.

#[test]
fn folio_cli_depends_on_none_of_bottles_crates() {
    let manifest =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    let mut section = "";
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line;
            continue;
        }
        if section.contains("dependencies") {
            let name = line.split(['=', '.', ' ']).next().unwrap_or_default();
            assert!(
                !name.starts_with("bottle"),
                "folio-cli depends on {name}: run bottle-cli instead"
            );
        }
    }
}
