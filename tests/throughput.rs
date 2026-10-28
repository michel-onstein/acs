//! Output throughput through a master (acs-m08z): a measurement, not a
//! gate, so it is ignored by default. Run it with
//!
//! ```text
//! cargo test --release --test throughput -- --ignored --nocapture
//! ```
//!
//! It streams `ACS_BENCH_MB` (default 512) MiB of program output through
//! one session and prints the wall-clock rate and the master's CPU time
//! per GiB, the number the master's copying shows up in.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use acs::proto::{Decoder, Mode, Msg};
use acs::testutil::{hello, TempDir};

#[test]
#[ignore = "a measurement: run with --ignored --nocapture"]
fn output_throughput() {
    let mb: u64 = std::env::var("ACS_BENCH_MB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(512);
    let total = mb << 20;
    let t = TempDir::new();
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_acs"));
    acs::master::spawn_with_env(&exe, t.path(), "tp", &[]).expect("spawn master");
    let sock = t.path().join("tp.sock");
    let mut s = UnixStream::connect(&sock).unwrap();
    let mut h = hello("tp", Mode::AttachOrCreate, "me");
    // Wait for a line on stdin before writing, so the clock and the CPU
    // count start together, and linger after so the CPU is read before
    // the master exits.
    let cmd = format!("head -c 1 >/dev/null; head -c {total} /dev/zero; sleep 3");
    h.command = vec!["/bin/sh".into(), "-c".into(), cmd];
    s.write_all(&Msg::Hello(h).to_bytes()).unwrap();
    let mut dec = Decoder::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut recv = |dec: &mut Decoder, s: &mut UnixStream| loop {
        if let Some(m) = dec.next_msg().unwrap() {
            return m;
        }
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0, "the master went away");
        dec.push(&buf[..n]);
    };
    loop {
        if let Msg::Welcome(_) = recv(&mut dec, &mut s) {
            break;
        }
    }
    let pid = acs::testutil::session_pid(&sock).unwrap();
    let cpu0 = acs::testutil::cpu_time(pid).unwrap();
    let t0 = Instant::now();
    s.write_all(
        &Msg::Input {
            seq: 0,
            bytes: b"x\n".to_vec(),
        }
        .to_bytes(),
    )
    .unwrap();
    // A pty in its default mode turns nothing in a run of NULs, so the
    // count is exact.
    let mut got = 0u64;
    while got < total {
        match recv(&mut dec, &mut s) {
            Msg::Data { bytes, .. } => got += bytes.iter().filter(|&&b| b == 0).count() as u64,
            Msg::Ping(n) => s.write_all(&Msg::Pong(n).to_bytes()).unwrap(),
            _ => {}
        }
    }
    let wall = t0.elapsed();
    let cpu = acs::testutil::cpu_time(pid).unwrap().saturating_sub(cpu0);
    let gib = total as f64 / (1u64 << 30) as f64;
    println!(
        "{mb} MiB in {wall:.2?}: {:.0} MiB/s; master CPU {cpu:.2?} ({:.2?}/GiB)",
        mb as f64 / wall.as_secs_f64(),
        Duration::from_secs_f64(cpu.as_secs_f64() / gib),
    );
    drop(s);
    acs::testutil::end_sessions_under(t.path());
}
