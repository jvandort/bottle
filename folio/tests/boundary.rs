//! folio runs bottle's code by running bottle-cli, never by linking it: folio depends on none of
//! bottle's crates but bottle-formats, the types of the JSON bottle-cli takes and gives. Libraries
//! both use (serde, sha2) are fine.

#[test]
fn folio_depends_on_none_of_bottles_crates_but_its_formats() {
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
                !name.starts_with("bottle") || name == "bottle-formats",
                "folio depends on {name}: run bottle-cli instead"
            );
        }
    }
}
