//! The **platform** half of the network watcher, driven by the kernel
//! (acs-4i2, DESIGN §5.3).
//!
//! Everything else about `netwatch.rs` is tested through the two stand-ins
//! acs-6p8 left — `ACS_NETWATCH_FIFO` for the kernel's hint and
//! `ACS_NETWATCH_NETS` for the machine's networks — which drive the
//! *decision* and say nothing about the platform. What was untested
//! anywhere is the assumption underneath it: that a real interface coming
//! and going makes the kernel emit a message the watcher's socket is
//! subscribed to **and** moves an address `netmatch::local_networks()`
//! reports. Both have to hold or the feature is silently dead, and its
//! absence looks exactly like a network that did not change.
//!
//! Here the kernel really does it. A container has a network namespace of
//! its own, so an interface can be created, addressed and torn down inside
//! it without touching the host: `scripts/test_linux.sh` runs this step as
//! root with `--cap-add NET_ADMIN`, and sets `ACS_NETNS_TEST=1` to say so.
//! Nothing in this process ever writes to the netlink socket — it is only
//! ever polled and read — so a descriptor that becomes readable after an
//! `ip` command is the kernel's own `NETLINK_ROUTE` multicast and can be
//! nothing else. `wait_readable` asserts that before every decision, which
//! is what separates "the watcher saw nothing" from "the watcher saw
//! something and judged it".
//!
//! Its own step rather than part of `cargo test`: adding and removing
//! interfaces changes what `getifaddrs` reports process-wide, and
//! `netwatch`'s and `netmatch`'s own tests read that while asserting it
//! holds still.
//!
//! The macOS `PF_ROUTE` half has no equivalent — a Mac's network cannot be
//! moved from inside CI — and stays a by-hand item in
//! `docs/VERIFICATION.md`, with the `-v` trace of acs-4i2 as what makes a
//! watcher that quietly stopped emitting visible there.

#![cfg(any(target_os = "linux", target_os = "android"))]

use std::process::Command;
use std::time::{Duration, Instant};

use acs::netwatch::NetWatch;

/// The interfaces this test makes. Names of its own, so a failure that
/// skips the cleanup cannot be mistaken for anything the container needs.
const ADDRESSED: &str = "acsnet0";
const BARE: &str = "acsnet1";

/// The address the first of them gets: TEST-NET-1 (RFC 5737), which
/// nothing routes, and a network `LocalNet::usable` accepts.
const ADDRESS: &str = "192.0.2.7/24";

/// How long the kernel is given to say something. Generous: the assertion
/// is that a message arrives at all, not when.
const T: Duration = Duration::from_secs(5);

fn ip(args: &[&str]) {
    let out = Command::new("ip")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("ip {args:?}: {e}"));
    assert!(
        out.status.success(),
        "ip {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn ip_quietly(args: &[&str]) {
    let _ = Command::new("ip").args(args).output();
}

/// The networks this machine can dial from, as `netwatch` writes them.
fn nets() -> Vec<String> {
    let mut n: Vec<String> = acs::netmatch::local_networks()
        .iter()
        .map(|n| format!("{}/{}", n.addr, n.prefix))
        .collect();
    n.sort();
    n
}

/// Wait until the watcher's descriptor has something to read, and fail if
/// it never does. Nothing in this process writes to that descriptor, so
/// whatever is there came from the kernel.
fn wait_readable(w: &NetWatch, what: &str) {
    let deadline = Instant::now() + T;
    loop {
        let mut fds = [acs::sys::pollfd(w.fd(), libc::POLLIN)];
        let _ = acs::sys::poll(&mut fds, 50);
        if fds[0].revents != 0 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the kernel said nothing about {what}"
        );
    }
}

/// Take the interfaces away however the test ends.
struct Cleanup;

impl Drop for Cleanup {
    fn drop(&mut self) {
        ip_quietly(&["link", "del", ADDRESSED]);
        ip_quietly(&["link", "del", BARE]);
    }
}

#[test]
fn the_kernel_drives_the_watcher() {
    if std::env::var_os("ACS_NETNS_TEST").is_none() {
        eprintln!(
            "skipped: needs a container of its own with CAP_NET_ADMIN — \
             scripts/test_linux.sh sets ACS_NETNS_TEST=1"
        );
        return;
    }
    // The real socket and the real `getifaddrs`, or this test is the fifo
    // knob over again and proves nothing about the platform.
    for var in ["ACS_NETWATCH_FIFO", "ACS_NETWATCH_NETS"] {
        assert!(
            std::env::var_os(var).is_none(),
            "{var} is set: this test needs the platform watcher, not a stand-in"
        );
    }
    let _cleanup = Cleanup;
    ip_quietly(&["link", "del", ADDRESSED]);
    ip_quietly(&["link", "del", BARE]);

    let w = NetWatch::new(false).expect("no NETLINK_ROUTE watcher");
    let before = nets();
    assert!(
        !before.iter().any(|n| n.starts_with("192.0.2.")),
        "the container is already on {ADDRESS}: {before:?}"
    );

    // An interface appears, down and with no address of its own. The
    // kernel broadcasts it (`RTM_NEWLINK`, the `RTMGRP_LINK` group the
    // watcher subscribes to) and the networks this machine could dial from
    // are the ones they were — which is acs-6p8's contract, here against
    // the real kernel rather than a file.
    ip(&["link", "add", ADDRESSED, "type", "dummy"]);
    wait_readable(&w, "an interface appearing");
    assert_eq!(nets(), before, "a down, unaddressed interface moved one");
    assert!(
        w.changed().is_none(),
        "an interface with no address is not a change"
    );

    // It comes up and gets an address: now something moved.
    ip(&["link", "set", ADDRESSED, "up"]);
    ip(&["addr", "add", ADDRESS, "dev", ADDRESSED]);
    wait_readable(&w, "an address arriving");
    let with = nets();
    assert!(
        with.contains(&ADDRESS.to_string()),
        "getifaddrs does not report {ADDRESS}: {with:?}"
    );
    // Acted on, as the client does with a change it can use: the networks
    // the watcher holds move here and nowhere else (acs-0n8), so the
    // assertions below are against this set and not the one before it.
    w.changed()
        .expect("an address this machine can dial from appeared")
        .acted();

    // A second bare interface: the kernel speaks again for nothing.
    ip(&["link", "add", BARE, "type", "dummy"]);
    wait_readable(&w, "a second interface appearing");
    assert_eq!(nets(), with, "a down, unaddressed interface moved one");
    assert!(w.changed().is_none(), "the same networks, so not a change");

    // And the address goes away with its interface.
    ip(&["link", "del", ADDRESSED]);
    wait_readable(&w, "an interface going away");
    assert_eq!(nets(), before, "{ADDRESS} outlived its interface");
    w.changed()
        .expect("the address this machine dialled from went")
        .acted();

    // And the kernel's second word about a change nobody acted on is the
    // same change (acs-0n8), here against the real thing: another address
    // arrives, the answer is read and dropped, and the next message —
    // whatever the kernel sends about the interface going away again —
    // finds the networks still held where the last acted-on change left
    // them.
    ip(&["link", "add", ADDRESSED, "type", "dummy"]);
    ip(&["link", "set", ADDRESSED, "up"]);
    ip(&["addr", "add", ADDRESS, "dev", ADDRESSED]);
    wait_readable(&w, "an address arriving again");
    assert!(
        w.changed().is_some(),
        "the address came back and that is a change"
    );
    ip(&["link", "del", ADDRESSED]);
    wait_readable(&w, "that interface going away");
    assert_eq!(nets(), before, "{ADDRESS} outlived its interface");
    assert!(
        w.changed().is_none(),
        "back on the networks the last acted-on change left held"
    );
}
