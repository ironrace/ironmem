//! Contract test for the intentionally opt-in Headroom dependency boundary.

use std::path::PathBuf;

fn ironmem_manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")
}

#[test]
fn headroom_compression_is_an_explicitly_optional_manifest_feature() {
    let manifest =
        std::fs::read_to_string(ironmem_manifest()).expect("ironmem Cargo.toml should be readable");

    assert!(
        manifest
            .contains("[features]\ndefault = []\nheadroom-compression = [\"dep:headroom-core\"]"),
        "Headroom compression must be explicitly opt-in and map only to headroom-core"
    );
    let headroom_dependency = manifest
        .lines()
        .find(|line| line.starts_with("headroom-core = "))
        .expect("headroom-core dependency must be declared");
    assert!(
        headroom_dependency.contains("git = \"https://github.com/headroomlabs-ai/headroom.git\""),
        "the dependency must be named headroom-core and use the reviewed upstream repository"
    );
    assert!(
        headroom_dependency.contains("rev = \"5bd2266f16bb351a7a7334e1c29c598d28187b1d\""),
        "the dependency must stay pinned to the reviewed commit"
    );
    assert!(
        headroom_dependency.contains("package = \"headroom-core\""),
        "the dependency must select the headroom-core package"
    );
    assert!(
        headroom_dependency.contains("default-features = false"),
        "headroom-core default features must remain disabled"
    );
    assert!(
        headroom_dependency.contains("optional = true"),
        "headroom-core must remain optional"
    );
}
