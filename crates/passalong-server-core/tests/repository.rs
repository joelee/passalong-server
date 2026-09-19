//! What the repository says of itself: its licence, in the one file and in
//! every manifest (PLAN-00006, REQ-01).

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// SHA-256 of the GNU Affero General Public License, version 3, as SPDX
/// distributes it (`AGPL-3.0-or-later.txt`, 34 020 bytes).
const AGPL_3_0: &str = "d8a6cc31abc16b6748c7a21f21611f5a1ec33f67d22ca23d7da1c19b95496bee";

/// The identifier every manifest carries (PLAN-00006 D-01).
const SPDX: &str = "AGPL-3.0-or-later";

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(path: &str) -> String {
    let path = root().join(path);
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

#[test]
fn the_licence_is_the_agpl_word_for_word() {
    let text = read("LICENSE");
    assert!(text.starts_with("GNU AFFERO GENERAL PUBLIC LICENSE\nVersion 3, 19 November 2007"));
    let digest: String = Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(digest, AGPL_3_0, "LICENSE is not the licence's own text");
    // Where the system has SPDX's copy, against that too.
    let spdx = Path::new("/usr/share/licenses/spdx/AGPL-3.0-or-later.txt");
    if let Ok(theirs) = std::fs::read_to_string(spdx) {
        assert_eq!(text, theirs);
    }
}

#[test]
fn every_manifest_names_the_licence_and_none_is_published() {
    let workspace = read("Cargo.toml");
    assert!(
        workspace.contains(&format!("license = \"{SPDX}\"")),
        "{workspace}"
    );
    assert!(workspace.contains("publish = false"));
    for manifest in [
        "crates/passalong-server-core/Cargo.toml",
        "crates/passalong-server-api/Cargo.toml",
        "crates/passalong-server-cli/Cargo.toml",
    ] {
        let text = read(manifest);
        assert!(text.contains("license.workspace = true"), "{manifest}");
        assert!(text.contains("publish.workspace = true"), "{manifest}");
        assert!(!text.contains("license-file"), "{manifest}");
    }
    assert!(!workspace.contains("license-file"));
}

#[test]
fn what_states_the_terms_states_these_terms() {
    for file in [
        "README.md",
        "CONTRIBUTING.md",
        "AGENTS.md",
        "Dockerfile",
        "deny.toml",
    ] {
        let text = read(file).to_lowercase();
        assert!(
            !text.contains("proprietary"),
            "{file} still says proprietary"
        );
        assert!(!text.contains("all rights reserved"), "{file}");
    }
    assert!(read("README.md").contains(SPDX));
}
