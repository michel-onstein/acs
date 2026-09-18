//! `cargo xtask package` turns a dist directory into release assets.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn packages_every_target_with_checksums_and_notes() {
    let root = std::env::temp_dir().join(format!("acs-package-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dist = root.join("dist");
    let out = root.join("assets");
    for t in ["aarch64-apple-darwin", "x86_64-unknown-linux-musl"] {
        std::fs::create_dir_all(dist.join(t)).unwrap();
        std::fs::write(dist.join(t).join("acs"), format!("binary for {t}")).unwrap();
    }
    // Not a target: no `acs` inside.
    std::fs::write(dist.join("payloads.bin"), b"blob").unwrap();
    std::fs::write(root.join("changes.md"), "- fix: something\n").unwrap();
    let readme = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../README.md");

    let st = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(["package", "--version", "v0.9.1", "--dist"])
        .arg(&dist)
        .arg("--out")
        .arg(&out)
        .arg("--readme")
        .arg(&readme)
        .arg("--changes")
        .arg(root.join("changes.md"))
        .status()
        .unwrap();
    assert!(st.success());

    let mut files: Vec<String> = std::fs::read_dir(&out)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().into_string().unwrap())
        .collect();
    files.sort();
    assert_eq!(
        files,
        [
            "NOTES.md",
            "SHA256SUMS",
            "acs-0.9.1-aarch64-apple-darwin.tar.gz",
            "acs-0.9.1-x86_64-unknown-linux-musl.tar.gz"
        ]
    );

    // The archive holds the binary (executable) and the README in a folder.
    let list = Command::new("tar")
        .arg("-tvzf")
        .arg(out.join("acs-0.9.1-x86_64-unknown-linux-musl.tar.gz"))
        .output()
        .unwrap();
    let list = String::from_utf8_lossy(&list.stdout);
    let acs_line = list
        .lines()
        .find(|l| l.ends_with("acs-0.9.1-x86_64-unknown-linux-musl/acs"))
        .unwrap_or_else(|| panic!("{list}"));
    assert!(acs_line.starts_with("-rwxr-xr-x"), "{acs_line}");
    assert!(
        list.contains("acs-0.9.1-x86_64-unknown-linux-musl/README.md"),
        "{list}"
    );

    // The checksums verify with the standard tool.
    let check = Command::new("shasum")
        .args(["-a", "256", "-c", "SHA256SUMS"])
        .current_dir(&out)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stdout)
    );

    let notes = std::fs::read_to_string(out.join("NOTES.md")).unwrap();
    assert!(notes.contains("acs 0.9.1"));
    assert!(notes.contains("- fix: something"));

    let _ = std::fs::remove_dir_all(&root);
}
