use std::path::{Path, PathBuf};

fn sources(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).expect("module directory must exist") {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(sources(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    files
}

#[test]
fn runtime_contracts_are_independent_of_tool_and_transport_implementations() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for path in sources(&source.join("runtime")) {
        let text = std::fs::read_to_string(&path).unwrap();
        for dependency in [
            "crate::bash",
            "crate::scripts",
            "crate::process",
            "tokio::process",
        ] {
            assert!(
                !text.contains(dependency),
                "{} imports implementation {dependency}",
                path.display()
            );
        }
    }
    assert!(
        source.join("scripts/wire.rs").is_file(),
        "fd3 encoding belongs to the script boundary"
    );
    assert!(
        source.join("scripts/fd3.rs").is_file(),
        "fd3 process adaptation belongs to the script boundary"
    );
    let root = std::fs::read_to_string(source.join("lib.rs")).unwrap();
    assert!(
        !root.contains("pub mod channel;"),
        "runtime contracts must have one public entry"
    );
    assert!(
        !root.contains("pub mod background;"),
        "runtime contracts must have one public entry"
    );
}
