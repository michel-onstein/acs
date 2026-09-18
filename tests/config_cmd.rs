//! `acs config …` against a temporary HOME (acs-iff, DESIGN §7.4).

mod common;

use std::path::PathBuf;
use std::process::Command;

use acs::testutil::TempDir;
use common::*;

struct Home {
    dir: TempDir,
}

impl Home {
    fn new() -> Home {
        Home {
            dir: TempDir::new(),
        }
    }

    fn local(&self) -> PathBuf {
        self.dir.path().join(".config/acs/config.yaml")
    }

    fn global(&self) -> PathBuf {
        self.dir.path().join("etc/acs/config.yaml")
    }

    /// `acs <args>` with this HOME, no XDG_CONFIG_HOME, and the global file
    /// under it too.
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(exe());
        c.args(args)
            .env("HOME", self.dir.path())
            .env_remove("XDG_CONFIG_HOME")
            .env("ACS_GLOBAL_CONFIG", self.global());
        c
    }

    /// Run; returns exit code, stdout, stderr.
    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let out = self.cmd(args).output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn ok(&self, args: &[&str]) -> String {
        let (code, out, err) = self.run(args);
        assert_eq!(code, 0, "acs {args:?}: {err}");
        out
    }

    fn fails(&self, args: &[&str], code: i32, want: &str) {
        let (c, out, err) = self.run(args);
        assert_eq!(c, code, "acs {args:?}: {out}{err}");
        assert!(err.contains(want), "acs {args:?}: {err}");
    }
}

#[test]
fn path_names_both_files() {
    let h = Home::new();
    let out = h.ok(&["config", "path"]);
    assert_eq!(
        out,
        format!(
            "global: {} (not found)\nlocal:  ~/.config/acs/config.yaml (not found)\n",
            h.global().display()
        )
    );
}

#[test]
fn host_add_list_and_remove() {
    let h = Home::new();
    assert_eq!(
        h.ok(&["config", "host", "add", "devbox", "devbox.lan"]),
        "added devbox.lan to devbox as its only host in ~/.config/acs/config.yaml\n"
    );
    assert_eq!(
        h.ok(&[
            "config",
            "host",
            "add",
            "devbox",
            "devbox.example.com",
            "--user",
            "me",
            "--no-reachability-check"
        ]),
        "added me@devbox.example.com to devbox as host 2 in ~/.config/acs/config.yaml\n"
    );
    assert_eq!(
        std::fs::read_to_string(h.local()).unwrap(),
        "hosts:\n  devbox:\n    - host: devbox.lan\n    - host: devbox.example.com\n      user: me\n      reachability_check: false\n"
    );
    let list = h.ok(&["config", "host", "list"]);
    let lines: Vec<&str> = list.lines().collect();
    assert_eq!(lines.len(), 3, "{list}");
    assert!(
        lines[1].contains("devbox.lan") && lines[1].contains("ping"),
        "{list}"
    );
    assert!(
        lines[2].contains("me") && lines[2].contains("none"),
        "{list}"
    );
    assert!(lines[2].ends_with("~/.config/acs/config.yaml:4"), "{list}");

    h.fails(
        &["config", "host", "add", "devbox", "devbox.lan"],
        2,
        "devbox.lan is already a host of devbox",
    );
    assert_eq!(
        h.ok(&["config", "host", "remove", "devbox", "devbox.lan"]),
        "removed devbox.lan from devbox in ~/.config/acs/config.yaml\n"
    );
    h.fails(
        &["config", "host", "remove", "devbox", "devbox.lan"],
        2,
        "devbox.lan is not a host of devbox in this file",
    );
    h.ok(&["config", "host", "remove", "devbox"]);
    assert_eq!(std::fs::read_to_string(h.local()).unwrap(), "");
    h.fails(
        &["config", "host", "remove", "devbox"],
        2,
        "no alias 'devbox'",
    );
}

#[test]
fn set_get_unset_keep_the_rest_of_the_file() {
    let h = Home::new();
    std::fs::create_dir_all(h.local().parent().unwrap()).unwrap();
    std::fs::write(
        h.local(),
        "# my settings\ninstall_on_remote: true   # for now\n\nhosts:\n    nas:   # 4-space indent\n        - host: nas.lan\n",
    )
    .unwrap();
    assert_eq!(h.ok(&["config", "get", "install_on_remote"]), "true\n");
    h.ok(&["config", "set", "install_on_remote", "false"]);
    assert_eq!(h.ok(&["config", "get", "install_on_remote"]), "false\n");
    // Comments, blank lines and order survive; indentation becomes 2.
    assert_eq!(
        std::fs::read_to_string(h.local()).unwrap(),
        "# my settings\ninstall_on_remote: false # for now\n\nhosts:\n  nas: # 4-space indent\n    - host: nas.lan\n"
    );
    h.ok(&["config", "unset", "install_on_remote"]);
    assert_eq!(h.ok(&["config", "get", "install_on_remote"]), "true\n");
    assert_eq!(
        std::fs::read_to_string(h.local()).unwrap(),
        "# my settings\n\nhosts:\n  nas: # 4-space indent\n    - host: nas.lan\n"
    );
}

#[test]
fn errors_are_clear() {
    let h = Home::new();
    h.fails(&["config"], 2, "acs config needs a command");
    h.fails(&["config", "get", "port"], 2, "unknown setting 'port'");
    h.fails(
        &["config", "set", "install_on_remote", "maybe"],
        2,
        "install_on_remote is true or false, not 'maybe'",
    );
    h.fails(&["config", "set", "user", "me"], 2, "acs config host add");
    h.fails(
        &["config", "unset", "install_on_remote"],
        2,
        "install_on_remote is not set in this file",
    );
    h.fails(
        &["config", "host", "add", "-x", "h"],
        2,
        "unknown option -x",
    );
    h.fails(
        &["config", "host", "add", "a", "me@h"],
        2,
        "bad host name 'me@h' (put a login name in 'user')",
    );
    assert!(!h.local().exists(), "a refused edit writes nothing");

    // A broken file is reported, and not overwritten.
    std::fs::create_dir_all(h.local().parent().unwrap()).unwrap();
    std::fs::write(h.local(), "install_on_remote: yes please\n").unwrap();
    h.fails(&["config", "show"], 2, "config.yaml:1: install_on_remote");
    h.fails(
        &["config", "host", "add", "a", "b"],
        2,
        "(not saved; fix the file by hand)",
    );
    assert_eq!(
        std::fs::read_to_string(h.local()).unwrap(),
        "install_on_remote: yes please\n"
    );
}

#[test]
fn global_edits_and_merged_views() {
    let h = Home::new();
    h.ok(&["config", "--global", "set", "install_on_remote", "false"]);
    h.ok(&["config", "host", "add", "devbox", "b.lan", "--global"]);
    h.ok(&["config", "host", "add", "devbox", "a.lan"]);
    assert_eq!(
        std::fs::read_to_string(h.global()).unwrap(),
        "install_on_remote: false\nhosts:\n  devbox:\n    - host: b.lan\n"
    );
    assert_eq!(h.ok(&["config", "get", "install_on_remote"]), "false\n");

    // The global entry comes first; show says where each value is from.
    let show = h.ok(&["config", "show"]);
    // Under HOME, so shown with ~.
    let g = "~/etc/acs/config.yaml";
    assert!(
        show.contains(&format!("install_on_remote: false # {g}:1\n")),
        "{show}"
    );
    assert!(
        show.contains(&format!(
            "  devbox:\n    - host: b.lan # {g}:4\n    - host: a.lan # ~/.config/acs/config.yaml:3\n"
        )),
        "{show}"
    );

    // Removing from the wrong file points at the right one.
    h.fails(
        &["config", "unset", "install_on_remote"],
        2,
        &format!("it is set in {g}:1 (use --global)"),
    );
    h.ok(&["config", "unset", "install_on_remote", "--global"]);
    assert_eq!(h.ok(&["config", "get", "install_on_remote"]), "true\n");
}

#[test]
fn an_unwritable_global_file_says_to_use_sudo() {
    if acs::sys::getuid() == 0 {
        return; // root writes anywhere
    }
    let h = Home::new();
    let dir = h.global().parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&dir).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    h.fails(
        &["config", "--global", "set", "install_on_remote", "false"],
        1,
        "(run with sudo for --global)",
    );
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn a_host_called_config_is_reachable_with_a_user_or_an_option_first() {
    let remote = Remote::installed();
    for args in [
        vec!["--transport-cmd", &remote.transport(), "config", "--list"],
        vec![
            "--transport-cmd",
            &remote.transport(),
            "me@config",
            "--list",
        ],
    ] {
        let out = acs_cmd().args(&args).output().unwrap();
        assert!(out.status.success(), "{args:?}");
        assert!(String::from_utf8_lossy(&out.stdout).starts_with("no sessions on"));
    }
}
