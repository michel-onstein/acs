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
        "aliases:\n  devbox:\n    - host: devbox.lan\n    - host: devbox.example.com\n      user: me\n      reachability_check: false\n"
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
        "# my settings\ninstall_on_remote: true   # for now\n\naliases:\n    nas:   # 4-space indent\n        - host: nas.lan\n",
    )
    .unwrap();
    assert_eq!(h.ok(&["config", "get", "install_on_remote"]), "true\n");
    h.ok(&["config", "set", "install_on_remote", "false"]);
    assert_eq!(h.ok(&["config", "get", "install_on_remote"]), "false\n");
    // Comments, blank lines and order survive; indentation becomes 2.
    assert_eq!(
        std::fs::read_to_string(h.local()).unwrap(),
        "# my settings\ninstall_on_remote: false # for now\n\naliases:\n  nas: # 4-space indent\n    - host: nas.lan\n"
    );
    h.ok(&["config", "unset", "install_on_remote"]);
    assert_eq!(h.ok(&["config", "get", "install_on_remote"]), "true\n");
    assert_eq!(
        std::fs::read_to_string(h.local()).unwrap(),
        "# my settings\n\naliases:\n  nas: # 4-space indent\n    - host: nas.lan\n"
    );
}

/// acs-tacx: a forward to the remote's IPv6 loopback is valid `-L`, but
/// written plain into the one-line list it broke the re-parse ("unexpected
/// text after the value"). Globally and on an alias it is now set and read
/// back intact.
#[test]
fn an_ipv6_local_forward_is_set_and_read_back() {
    let h = Home::new();
    h.ok(&["config", "set", "local_forwards", "8080:[::1]:80"]);
    assert_eq!(
        h.ok(&["config", "get", "local_forwards"]),
        "8080:[::1]:80\n"
    );
    assert_eq!(
        std::fs::read_to_string(h.local()).unwrap(),
        "local_forwards: [\"8080:[::1]:80\"]\n"
    );
    h.ok(&[
        "config",
        "set",
        "local_forwards",
        "8080:[::1]:80,9090:localhost:90",
    ]);
    assert_eq!(
        h.ok(&["config", "get", "local_forwards"]),
        "8080:[::1]:80,9090:localhost:90\n"
    );

    h.ok(&["config", "host", "add", "d", "devbox.lan"]);
    h.ok(&[
        "config",
        "host",
        "set",
        "d",
        "local_forwards",
        "5432:[::1]:5432",
    ]);
    let file = std::fs::read_to_string(h.local()).unwrap();
    assert!(
        file.contains("local_forwards: [\"5432:[::1]:5432\"]"),
        "{file}"
    );
}

/// acs-9yv: `local_networks` on a host entry, through the real binary —
/// added, shown, and refused where the setting used to live.
#[test]
fn local_networks_is_added_to_a_host_and_shown_as_a_list() {
    let h = Home::new();
    assert_eq!(
        h.ok(&[
            "config",
            "host",
            "add",
            "devbox",
            "devbox.lan",
            "--local-networks",
            "172.16.0.0/16,fd00::/48",
        ]),
        "added devbox.lan to devbox as its only host in ~/.config/acs/config.yaml\n"
    );
    assert_eq!(
        std::fs::read_to_string(h.local()).unwrap(),
        "aliases:\n  devbox:\n    - host: devbox.lan\n      \
         local_networks: [172.16.0.0/16, fd00::/48]\n"
    );
    let show = h.ok(&["config", "show"]);
    assert!(
        show.contains("      local_networks: [172.16.0.0/16, fd00::/48]\n"),
        "{show}"
    );
    // A bad network is refused, naming what was wrong, and changes nothing.
    let before = std::fs::read_to_string(h.local()).unwrap();
    h.fails(
        &[
            "config",
            "host",
            "add",
            "devbox",
            "other.lan",
            "--local-networks",
            "172.16.0.0",
        ],
        2,
        "expected a network such as 172.16.0.0/16 or fd00::/48, found '172.16.0.0'",
    );
    h.fails(
        &[
            "config",
            "host",
            "add",
            "devbox",
            "other.lan",
            "--local-networks",
            "10.0.0.0/8,0.0.0.0/0",
        ],
        2,
        "a /0 network is every address",
    );
    // It is no longer a global or a per-alias setting (acs-9yv): both say
    // so rather than writing it.
    h.fails(
        &["config", "set", "local_networks", "172.16.0.0/16"],
        2,
        "unknown setting 'local_networks'",
    );
    h.fails(
        &[
            "config",
            "host",
            "set",
            "devbox",
            "local_networks",
            "10.0.0.0/8",
        ],
        2,
        "unknown alias setting 'local_networks'",
    );
    // A file that still carries the old form is a plain unknown key.
    std::fs::write(h.local(), "local_networks: [172.16.0.0/16]\n").unwrap();
    h.fails(&["config", "show"], 2, "unknown setting 'local_networks'");
    std::fs::write(h.local(), before).unwrap();
    assert_eq!(h.ok(&["config", "get", "aliases"]).lines().count(), 2);
}

/// Regression (acs-q8e): a configuration kept in dotfiles is reached through
/// a symlink. An edit must change the file it points at and leave the link.
#[test]
fn an_edit_writes_through_a_symlink() {
    let h = Home::new();
    let dotfiles = h.dir.path().join("dotfiles");
    std::fs::create_dir_all(&dotfiles).unwrap();
    let target = dotfiles.join("acs.yaml");
    std::fs::write(&target, "install_on_remote: true\n").unwrap();
    std::fs::create_dir_all(h.local().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&target, h.local()).unwrap();

    h.ok(&["config", "set", "install_on_remote", "false"]);
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "install_on_remote: false\n",
        "the file the link points at was not edited"
    );
    assert!(
        std::fs::symlink_metadata(h.local())
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link was replaced by a regular file"
    );
    assert_eq!(h.ok(&["config", "get", "install_on_remote"]), "false\n");
}

/// Regression (acs-5mmu): a dotfiles link made before the file it points at
/// exists. `canonicalize` fails on a dangling link, and the rename used to
/// land on the link path, replacing the link with a regular file.
#[test]
fn the_first_edit_writes_through_a_dangling_symlink() {
    let h = Home::new();
    let target = h.dir.path().join("dotfiles/acs.yaml");
    std::fs::create_dir_all(h.local().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&target, h.local()).unwrap();

    h.ok(&["config", "set", "install_on_remote", "false"]);
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "install_on_remote: false\n",
        "the file the link points at was not created"
    );
    assert!(
        std::fs::symlink_metadata(h.local())
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link was replaced by a regular file"
    );
}

/// Regression (acs-5mmu): an edit keeps a leading `---` and a closing `...`.
#[test]
fn an_edit_keeps_the_document_markers() {
    let h = Home::new();
    std::fs::create_dir_all(h.local().parent().unwrap()).unwrap();
    std::fs::write(h.local(), "---\ninstall_on_remote: true\n...\n").unwrap();
    h.ok(&["config", "set", "install_on_remote", "false"]);
    assert_eq!(
        std::fs::read_to_string(h.local()).unwrap(),
        "---\ninstall_on_remote: false\n...\n"
    );
}

/// Regression (acs-5mmu): a refused edit named a line of the text it did
/// not save — line 6 of a five-line file — rather than the file on disk.
#[test]
fn a_refused_edit_does_not_cite_a_line_of_the_unsaved_text() {
    let h = Home::new();
    std::fs::create_dir_all(h.local().parent().unwrap()).unwrap();
    let before = "# hosts\naliases:\n  web:\n    hosts:\n      - host: web.example\n";
    std::fs::write(h.local(), before).unwrap();
    h.fails(
        &["config", "host", "add", "nas", "me@h"],
        2,
        "config.yaml: aliases.nas: host: bad host name 'me@h'",
    );
    assert_eq!(std::fs::read_to_string(h.local()).unwrap(), before);
}

/// Regression (acs-vz64): a value refused from the edit itself said to
/// fix the file by hand, though the file was fine. Only a file that is
/// refused on its own gets that advice.
#[test]
fn a_refused_edit_says_fix_by_hand_only_for_a_bad_file() {
    let h = Home::new();
    std::fs::create_dir_all(h.local().parent().unwrap()).unwrap();
    let good = "aliases:\n  web:\n    hosts:\n      - host: web.example\n";
    std::fs::write(h.local(), good).unwrap();
    let (c, _, err) = h.run(&["config", "host", "add", "nas", "me@h"]);
    assert_eq!(c, 2, "{err}");
    assert!(
        err.trim_end()
            .ends_with("bad host name 'me@h' (put a login name in 'user') (not saved)"),
        "{err}"
    );
    assert!(!err.contains("by hand"), "{err}");
    assert_eq!(std::fs::read_to_string(h.local()).unwrap(), good);

    // The same edit on a file that is refused without it.
    let bad = format!("install_on_remote: yes please\n{good}");
    std::fs::write(h.local(), &bad).unwrap();
    h.fails(
        &["config", "host", "add", "nas", "me@h"],
        2,
        "(not saved; fix the file by hand)",
    );
    assert_eq!(std::fs::read_to_string(h.local()).unwrap(), bad);
}

/// acs-siqo: an edit reads each file once and hands it to every check, and
/// gives the answers it gave when each check read the files itself. A host
/// set needs both files whole and reports the first broken one in file
/// order (global, then local); any other edit leaves a broken other file
/// alone, as before; and the hint for a refused unset still finds the
/// setting in the other file.
#[test]
fn an_edit_reads_each_file_once_and_answers_as_before() {
    let h = Home::new();
    h.ok(&["config", "host", "add", "devbox", "a.lan"]);
    std::fs::create_dir_all(h.global().parent().unwrap()).unwrap();

    // A broken global file: the host set names it, though the edit is local.
    std::fs::write(h.global(), "aliases: [\n").unwrap();
    h.fails(
        &["config", "host", "set", "devbox", "persist", "false"],
        2,
        "etc/acs/config.yaml:1",
    );
    // Both broken: still the global one, read first.
    let local = std::fs::read_to_string(h.local()).unwrap();
    std::fs::write(h.local(), "aliases: [\n").unwrap();
    h.fails(
        &["config", "host", "set", "devbox", "persist", "false"],
        2,
        "etc/acs/config.yaml:1",
    );
    std::fs::write(h.local(), &local).unwrap();
    // Any other edit of the local file does not need the global one.
    h.ok(&["config", "set", "install_on_remote", "false"]);
    assert_eq!(std::fs::read_to_string(h.global()).unwrap(), "aliases: [\n");

    // A good global file: the host set goes through, and an unset of what
    // only the global file sets points there.
    std::fs::write(h.global(), "update_check: false\n").unwrap();
    h.ok(&["config", "host", "set", "devbox", "persist", "false"]);
    assert_eq!(
        std::fs::read_to_string(h.local()).unwrap(),
        "aliases:\n  devbox:\n    persist: false\n    hosts:\n      - host: a.lan\ninstall_on_remote: false\n"
    );
    h.fails(
        &["config", "unset", "update_check"],
        2,
        "it is set in ~/etc/acs/config.yaml:1 (use --global)",
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
        "install_on_remote: false\naliases:\n  devbox:\n    - host: b.lan\n"
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
fn identity_files_for_a_host_and_for_an_alias() {
    // acs-mbd: the global file lists the hosts, the local one picks the key.
    let h = Home::new();
    h.fails(
        &[
            "config",
            "host",
            "set",
            "devbox",
            "identity_file",
            "~/.ssh/k",
        ],
        2,
        "no alias 'devbox' (add its first host with: acs config host add devbox <host>)",
    );
    assert_eq!(
        h.ok(&[
            "config",
            "--global",
            "host",
            "add",
            "devbox",
            "devbox.lan",
            "--identity-file",
            "/etc/acs/keys/lan"
        ]),
        // The global file is under HOME here, so shown with ~.
        "added devbox.lan to devbox as its only host, with identity_file /etc/acs/keys/lan in ~/etc/acs/config.yaml\n"
    );
    h.ok(&[
        "config",
        "--global",
        "host",
        "add",
        "devbox",
        "devbox.example.com",
    ]);
    assert_eq!(
        h.ok(&[
            "config",
            "host",
            "set",
            "devbox",
            "identity_file",
            "~/.ssh/id_devbox"
        ]),
        "set identity_file of devbox to ~/.ssh/id_devbox in ~/.config/acs/config.yaml\n"
    );
    assert_eq!(
        std::fs::read_to_string(h.local()).unwrap(),
        "aliases:\n  devbox:\n    identity_file: ~/.ssh/id_devbox\n"
    );

    // Each host with the key it is reached with.
    let list = h.ok(&["config", "host", "list"]);
    let lines: Vec<&str> = list.lines().collect();
    assert!(lines[0].contains("IDENTITY"), "{list}");
    assert!(lines[1].contains(" /etc/acs/keys/lan "), "{list}");
    assert!(lines[2].contains(" ~/.ssh/id_devbox "), "{list}");
    let show = h.ok(&["config", "show"]);
    assert!(
        show.contains(
            "  devbox:\n    identity_file: ~/.ssh/id_devbox # ~/.config/acs/config.yaml:3\n    hosts:\n"
        ),
        "{show}"
    );
    assert!(
        show.contains("        identity_file: /etc/acs/keys/lan\n"),
        "{show}"
    );

    // The hosts cannot go from under the key: the client would refuse it.
    h.fails(
        &["config", "--global", "host", "remove", "devbox"],
        2,
        "config.yaml:2: aliases.devbox: no hosts listed once this edit is made (not saved)",
    );
    assert!(std::fs::read_to_string(h.global())
        .unwrap()
        .contains("devbox.example.com"));

    // Unsetting it where it is not points at where it is.
    h.fails(
        &["config", "--global", "host", "unset", "devbox", "identity_file"],
        2,
        "identity_file of devbox is not set in this file; it is set in ~/.config/acs/config.yaml:3 (use no --global)",
    );
    h.ok(&["config", "host", "unset", "devbox", "identity_file"]);
    assert_eq!(std::fs::read_to_string(h.local()).unwrap(), "");
    assert!(!h.ok(&["config", "host", "list"]).contains("id_devbox"));
}

/// acs-m8ts: an identity file ending in U+00A0 is written quoted and read
/// back whole — it used to be saved bare and read back without it.
#[test]
fn an_identity_file_ending_in_a_no_break_space_reads_back_whole() {
    let h = Home::new();
    h.ok(&[
        "config",
        "host",
        "add",
        "a",
        "b",
        "--identity-file",
        "k\u{a0}",
    ]);
    assert_eq!(
        std::fs::read_to_string(h.local()).unwrap(),
        "aliases:\n  a:\n    - host: b\n      identity_file: \"k\u{a0}\"\n"
    );
    let list = h.ok(&["config", "host", "list"]);
    assert!(list.contains(" k\u{a0}"), "{list:?}");
}

#[test]
fn redraw_on_reconnect_globally_and_for_an_alias() {
    // acs-ome: off for everything, back on for one alias.
    let h = Home::new();
    assert_eq!(h.ok(&["config", "get", "redraw_on_reconnect"]), "true\n");
    h.ok(&["config", "set", "redraw_on_reconnect", "false"]);
    assert_eq!(h.ok(&["config", "get", "redraw_on_reconnect"]), "false\n");
    h.ok(&["config", "host", "add", "devbox", "devbox.lan"]);
    assert_eq!(
        h.ok(&[
            "config",
            "host",
            "set",
            "devbox",
            "redraw_on_reconnect",
            "true"
        ]),
        "set redraw_on_reconnect of devbox to true in ~/.config/acs/config.yaml\n"
    );
    assert_eq!(
        std::fs::read_to_string(h.local()).unwrap(),
        "redraw_on_reconnect: false\naliases:\n  devbox:\n    redraw_on_reconnect: true\n    hosts:\n      - host: devbox.lan\n"
    );
    let show = h.ok(&["config", "show"]);
    let l = "~/.config/acs/config.yaml";
    assert!(
        show.contains(&format!("redraw_on_reconnect: false # {l}:1\n")),
        "{show}"
    );
    assert!(
        show.contains(&format!(
            "  devbox:\n    redraw_on_reconnect: true # {l}:4\n    hosts:\n"
        )),
        "{show}"
    );
    h.fails(
        &[
            "config",
            "host",
            "set",
            "devbox",
            "redraw_on_reconnect",
            "1",
        ],
        2,
        "redraw_on_reconnect is true or false, not '1'",
    );
    h.ok(&["config", "host", "unset", "devbox", "redraw_on_reconnect"]);
    h.ok(&["config", "unset", "redraw_on_reconnect"]);
    assert_eq!(
        std::fs::read_to_string(h.local()).unwrap(),
        "aliases:\n  devbox:\n    - host: devbox.lan\n"
    );
}

#[test]
fn an_empty_value_means_the_same_globally_and_for_an_alias() {
    // acs-cdal: `host set <alias> <key> ''` was refused as "bad arguments"
    // whatever the setting, while `set <key> ''` was not.
    let h = Home::new();
    h.ok(&["config", "host", "add", "devbox", "devbox.lan"]);
    h.ok(&["config", "set", "local_forwards", "8080:localhost:80"]);
    assert_eq!(
        h.ok(&["config", "host", "set", "devbox", "local_forwards", ""]),
        "set local_forwards of devbox to none in ~/.config/acs/config.yaml\n"
    );
    assert_eq!(
        h.ok(&["config", "set", "local_forwards", ""]),
        "set local_forwards to none in ~/.config/acs/config.yaml\n"
    );
    // A setting an empty value means nothing to refuses it by name, the
    // same way in both forms.
    let want = "persist is true or false, not ''";
    h.fails(&["config", "set", "persist", ""], 2, want);
    h.fails(&["config", "host", "set", "devbox", "persist", ""], 2, want);
    h.fails(
        &["config", "host", "set", "devbox", "identity_file", ""],
        2,
        "identity_file needs a key file, not an empty value",
    );
    assert_eq!(
        std::fs::read_to_string(h.local()).unwrap(),
        "aliases:\n  devbox:\n    local_forwards: none\n    hosts:\n      - host: devbox.lan\nlocal_forwards: none\n"
    );
}

#[test]
fn an_alias_with_a_key_but_no_hosts_is_an_error() {
    let h = Home::new();
    std::fs::create_dir_all(h.local().parent().unwrap()).unwrap();
    std::fs::write(
        h.local(),
        "aliases:\n  devbox:\n    identity_file: ~/.ssh/k\n",
    )
    .unwrap();
    h.fails(
        &["config", "show"],
        2,
        "config.yaml:2: aliases.devbox: no hosts listed",
    );
    // The client refuses it too, before any ssh.
    let (code, _, err) = h.run(&["devbox"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("aliases.devbox: no hosts listed"), "{err}");
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

/// Reserved words (`config`, `upgrade`, `list`) only as the very first
/// argument: a host of that name is reachable with a user or an option
/// before it — here listed, as `acs list <host>`.
#[test]
fn a_host_called_config_is_reachable_with_a_user_or_an_option_first() {
    let remote = Remote::installed();
    let t = remote.transport();
    for args in [
        vec!["list", "--transport-cmd", &t, "config"],
        vec!["list", "--transport-cmd", &t, "me@config"],
        vec!["list", "--transport-cmd", &t, "list"],
        vec!["list", "--transport-cmd", &t, "me@list"],
    ] {
        let out = acs_cmd().args(&args).output().unwrap();
        assert!(out.status.success(), "{args:?}");
        assert!(String::from_utf8_lossy(&out.stdout).starts_with("no sessions on"));
    }
}

/// Regression (acs-36l0): with HOME and XDG_CONFIG_HOME both unset, the
/// local file used to be `.config/acs/config.yaml` under the working
/// directory — `acs config set` made it there, and the client read a
/// different configuration in every directory. Now `acs config` refuses,
/// and the client reads the global file alone.
#[test]
fn no_home_means_no_local_file() {
    let cwd = TempDir::new();
    let run = |args: &[&str]| {
        let out = Command::new(exe())
            .args(args)
            .current_dir(cwd.path())
            .env_remove("HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env("ACS_GLOBAL_CONFIG", format!("{NO_CONFIG}/global.yaml"))
            .env("ACS_NO_UPDATE_CHECK", "1")
            .output()
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    for args in [
        &["config", "path"][..],
        &["config", "set", "install_on_remote", "false"],
        &["config", "host", "add", "d", "a"],
    ] {
        let (code, out, err) = run(args);
        assert_eq!(code, 2, "acs {args:?}: {out}{err}");
        assert!(err.contains("HOME is not set"), "acs {args:?}: {err}");
    }
    assert!(
        !cwd.path().join(".config").exists(),
        "acs config wrote under the working directory"
    );
    let (code, out, err) = run(&["config", "help"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("acs config"), "{out}");

    // A file where the old path pointed is not read by the client.
    let stray = cwd.path().join(".config/acs/config.yaml");
    std::fs::create_dir_all(stray.parent().unwrap()).unwrap();
    std::fs::write(&stray, "nonsense: [\n").unwrap();
    let (_, out, err) = run(&["list"]);
    assert!(!err.contains("config.yaml:"), "{out}{err}");
    assert!(err.contains("no configuration"), "{out}{err}");
}

/// acs-gydj: every row of the settings table goes through the commands the
/// same way — `set`, `get` and `unset` for a top-level one, `host set`,
/// `show` and `host unset` for an alias's — so a new row needs no code of
/// its own in `acs config`.
#[test]
fn every_setting_in_the_table_round_trips() {
    use acs::config::{Kind, SETTINGS};
    let h = Home::new();
    h.ok(&["config", "host", "add", "d", "d.lan"]);
    let pristine = std::fs::read_to_string(h.local()).unwrap();
    for d in SETTINGS {
        let value = |current: &str| match d.kind {
            // Not the default, so `get` shows the edit took.
            Kind::Bool if current.trim() == "true" => "false",
            Kind::Bool => "true",
            Kind::Duration { .. } => "1500ms",
            Kind::Forwards => "8080:localhost:80",
            Kind::Path => "~/.ssh/k",
        };
        if d.global.is_some() {
            let before = h.ok(&["config", "get", d.key]);
            let v = value(&before);
            h.ok(&["config", "set", d.key, v]);
            assert_eq!(h.ok(&["config", "get", d.key]).trim(), v, "{}", d.key);
            h.ok(&["config", "unset", d.key]);
            assert_eq!(h.ok(&["config", "get", d.key]), before, "{}", d.key);
        } else {
            h.fails(&["config", "set", d.key, "x"], 2, "unknown setting");
        }
        if d.alias.is_some() {
            let v = value("");
            h.ok(&["config", "host", "set", "d", d.key, v]);
            let show = h.ok(&["config", "show"]);
            let line = format!("{}: {v}", d.key);
            let line = match d.kind {
                Kind::Forwards => format!("{}: [{v}]", d.key),
                _ => line,
            };
            assert!(show.contains(&line), "{} not in:\n{show}", d.key);
            h.ok(&["config", "host", "unset", "d", d.key]);
        } else {
            h.fails(
                &["config", "host", "set", "d", d.key, "true"],
                2,
                "unknown alias setting",
            );
        }
        assert_eq!(
            std::fs::read_to_string(h.local()).unwrap(),
            pristine,
            "{}",
            d.key
        );
    }
}
