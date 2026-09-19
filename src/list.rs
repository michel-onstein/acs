//! `acs list <host>` (DESIGN §4.3): ask the remote proxy for every
//! session's STATUS and print a table. `acs list` asks every alias in the
//! configuration at once (DESIGN §7.3).

use std::os::fd::AsRawFd;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::cli::ClientArgs;
use crate::client::{self, code};
use crate::proto::{Decoder, Marker, Msg, StatusInfo};
use crate::ssh::{self, Call};
use crate::sys;

/// Why a host's sessions could not be listed.
#[derive(Debug)]
pub(crate) enum Failure {
    /// No connection, or no answer in time.
    Unreachable(String),
    /// The proxy answered with something that is not a STATUS reply.
    BadReply(String),
}

impl Failure {
    /// What to tell the user about `host`.
    pub(crate) fn message(&self, host: &str) -> String {
        match self {
            Failure::Unreachable(e) => e.clone(),
            Failure::BadReply(e) => format!("bad reply from {host}: {e}"),
        }
    }

    /// The client's exit status for it.
    pub(crate) fn code(&self) -> u8 {
        match self {
            Failure::Unreachable(_) => code::UNREACHABLE,
            Failure::BadReply(_) => code::ERROR,
        }
    }
}

pub fn run(args: &ClientArgs) -> ExitCode {
    let host = args.host_name();
    match query(args, Call::Side, client::answer_timeout(false)) {
        Ok(Some(sessions)) => print!("{}", render(host, &sessions, sys::unix_now())),
        Ok(None) => println!("{}", not_installed(host)),
        Err(f) => {
            eprintln!("acs: {}", f.message(host));
            return ExitCode::from(f.code());
        }
    }
    ExitCode::SUCCESS
}

/// `acs list`: every alias, each resolved as a connection would be
/// (DESIGN §7.3) and asked in parallel, so a slow or dead host holds up
/// only its own line. Exits 0 if every host answered, 255 if any did not.
pub fn run_all(args: &ClientArgs) -> ExitCode {
    let aliases: Vec<&str> = args.config.hosts.iter().map(|a| a.name.as_str()).collect();
    if aliases.is_empty() {
        eprintln!(
            "acs: no host aliases in the configuration: list one host with acs list <host>, or add an alias with acs config host add <alias> <host>"
        );
        return ExitCode::from(code::USAGE);
    }
    // Nobody can type a password into several ssh at once (Call::Batch), so
    // a host gets the redial's limit rather than the first connection's.
    let timeout = client::answer_timeout(true);
    let answers: Vec<_> = std::thread::scope(|s| {
        let asks: Vec<_> = aliases
            .iter()
            .map(|&alias| s.spawn(move || ask(args, alias, timeout)))
            .collect();
        asks.into_iter()
            .map(|t| t.join().expect("a list thread panicked"))
            .collect()
    });

    let mut listed = Vec::new();
    let mut quiet = String::new();
    let mut failed = Vec::new();
    for (alias, answer) in aliases.into_iter().zip(answers) {
        match answer {
            Ok(Some(sessions)) if sessions.is_empty() => {
                quiet.push_str(&format!("no sessions on {alias}\n"))
            }
            Ok(Some(sessions)) => listed.push((alias, sessions)),
            Ok(None) => quiet.push_str(&format!("{}\n", not_installed(alias))),
            Err(Failure::Unreachable(e)) => failed.push(format!("acs: {alias}: {e}")),
            Err(Failure::BadReply(e)) => failed.push(format!("acs: bad reply from {alias}: {e}")),
        }
    }
    print!("{}{quiet}", render_all(&listed, sys::unix_now()));
    for f in &failed {
        eprintln!("{f}");
    }
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(code::UNREACHABLE)
    }
}

/// Resolve `alias` and ask the host it stands for now.
fn ask(
    base: &ClientArgs,
    alias: &str,
    timeout: Duration,
) -> Result<Option<Vec<StatusInfo>>, Failure> {
    let mut args = base.clone();
    client::resolve_alias(&mut args, alias).map_err(Failure::Unreachable)?;
    query(&args, Call::Batch, timeout)
}

fn not_installed(host: &str) -> String {
    format!(
        "no sessions on {host} (acs {} is not installed there)",
        crate::VERSION
    )
}

/// Ask the host's proxy (`_proxy --list`) for every session's STATUS, all
/// within `timeout`; `Ok(None)` if acs of our version is not installed
/// there.
pub(crate) fn query(
    args: &ClientArgs,
    call: Call,
    timeout: Duration,
) -> Result<Option<Vec<StatusInfo>>, Failure> {
    let deadline = Instant::now() + timeout;
    let remote = ssh::remote_acs(crate::VERSION, &["_proxy", "--list"]);
    let (link, marker) = client::dial(args, call, &remote, timeout)
        .map_err(|e| Failure::Unreachable(e.to_string()))?;
    let rest = match marker {
        Marker::Ready { rest, .. } => rest,
        Marker::Need { .. } => {
            link.close();
            return Ok(None);
        }
    };
    let mut dec = Decoder::new();
    dec.push(&rest);
    let mut sessions = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    let from = link.from_fd().as_raw_fd();
    loop {
        match dec.next_msg() {
            Ok(Some(Msg::StatusReply(s))) => {
                sessions.push(s);
                continue;
            }
            Ok(Some(_)) => continue,
            Ok(None) => {}
            Err(e) => {
                link.close();
                return Err(Failure::BadReply(e.to_string()));
            }
        }
        // A host that goes quiet after its marker must not hold us forever.
        let left = deadline.saturating_duration_since(Instant::now());
        let mut p = [sys::pollfd(from, libc::POLLIN)];
        let ready = if left.is_zero() {
            Ok(0)
        } else {
            sys::poll(&mut p, left.as_millis().min(i32::MAX as u128) as i32)
        };
        match ready {
            Ok(0) if Instant::now() < deadline => continue, // a signal cut the wait short
            Ok(0) => {
                link.close();
                return Err(Failure::Unreachable(format!(
                    "no answer from {} within {} s",
                    args.transport.destination,
                    timeout.as_secs_f32()
                )));
            }
            Ok(_) => {}
            Err(e) => {
                link.close();
                return Err(Failure::Unreachable(e.to_string()));
            }
        }
        match sys::read(from, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => dec.push(&buf[..n]),
        }
    }
    link.close();
    Ok(Some(sessions))
}

/// Compact durations in one unit: `42s`, `7m`, `3h`, `12d`.
pub fn span(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

const HEAD: [&str; 6] = ["NAME", "STATE", "WHO", "IDLE", "AGE", "COMMAND"];

/// One session's cells, in the order of [`HEAD`].
fn cells(s: &StatusInfo, now: u64) -> Vec<String> {
    vec![
        s.name.clone(),
        if s.attached { "attached" } else { "detached" }.to_string(),
        if s.identity.is_empty() {
            "-".to_string()
        } else if s.attached {
            s.identity.clone()
        } else {
            format!("({})", s.identity)
        },
        span(s.idle_secs),
        span(now.saturating_sub(s.created_at)),
        s.command.clone(),
    ]
}

/// `rows` under `head`, each column as wide as its widest cell (the last,
/// the command, is not padded).
fn table(head: &[&str], rows: &[Vec<String>]) -> String {
    table_lines(head, rows).concat()
}

/// The lines of [`table`], each ending in `\n`: the heading, then a line
/// per row.
fn table_lines(head: &[&str], rows: &[Vec<String>]) -> Vec<String> {
    let mut width: Vec<usize> = head.iter().map(|h| h.len()).collect();
    for r in rows {
        for (w, c) in width.iter_mut().zip(r.iter()) {
            *w = (*w).max(c.chars().count());
        }
    }
    let line = |cells: &[&str]| -> String {
        let mut s = String::new();
        for (i, c) in cells.iter().enumerate() {
            if i == cells.len() - 1 {
                s.push_str(c);
            } else {
                s.push_str(&format!("{c:<w$}  ", w = width[i]));
            }
        }
        s.trim_end().to_string() + "\n"
    };
    let mut out = vec![line(head)];
    for r in rows {
        out.push(line(&r.iter().map(String::as_str).collect::<Vec<_>>()));
    }
    out
}

/// The `acs list` table of `sessions` as lines without their `\n`: the
/// heading first (the session menu, DESIGN §4.4).
pub(crate) fn lines(sessions: &[StatusInfo], now: u64) -> Vec<String> {
    let rows: Vec<Vec<String>> = sessions.iter().map(|s| cells(s, now)).collect();
    table_lines(&HEAD, &rows)
        .into_iter()
        .map(|mut l| {
            l.pop();
            l
        })
        .collect()
}

/// The table `acs list <host>` prints.
pub fn render(host: &str, sessions: &[StatusInfo], now: u64) -> String {
    if sessions.is_empty() {
        return format!("no sessions on {host}\n");
    }
    let rows: Vec<Vec<String>> = sessions.iter().map(|s| cells(s, now)).collect();
    table(&HEAD, &rows)
}

/// The table `acs list` prints: the sessions of every host that has any,
/// under a HOST column; empty if none has.
pub fn render_all(hosts: &[(&str, Vec<StatusInfo>)], now: u64) -> String {
    let rows: Vec<Vec<String>> = hosts
        .iter()
        .flat_map(|(host, sessions)| {
            sessions.iter().map(move |s| {
                let mut row = vec![host.to_string()];
                row.extend(cells(s, now));
                row
            })
        })
        .collect();
    if rows.is_empty() {
        return String::new();
    }
    let mut head = vec!["HOST"];
    head.extend(HEAD);
    table(&head, &rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(
        name: &str,
        attached: bool,
        who: &str,
        idle: u64,
        created: u64,
        cmd: &str,
    ) -> StatusInfo {
        StatusInfo {
            name: name.into(),
            attached,
            identity: who.into(),
            creator: String::new(),
            created_at: created,
            idle_secs: idle,
            command: cmd.into(),
            size: Default::default(),
            version: "0.1.0".into(),
            pid: 1,
        }
    }

    #[test]
    fn spans() {
        assert_eq!(span(0), "0s");
        assert_eq!(span(59), "59s");
        assert_eq!(span(60), "1m");
        assert_eq!(span(7200), "2h");
        assert_eq!(span(3 * 86_400 + 5), "3d");
    }

    #[test]
    fn table_of_one_host() {
        let now = 1_000_000;
        let t = render(
            "devbox",
            &[
                info("main", true, "michel@mbp", 3, now - 7200, "/bin/zsh -l"),
                info("2", false, "alice@laptop", 900, now - 60, "htop"),
                info("x", false, "", 0, now, "sh"),
            ],
            now,
        );
        assert_eq!(
            t,
            "NAME  STATE     WHO             IDLE  AGE  COMMAND\n\
             main  attached  michel@mbp      3s    2h   /bin/zsh -l\n\
             2     detached  (alice@laptop)  15m   1m   htop\n\
             x     detached  -               0s    0s   sh\n"
        );
        assert_eq!(render("h", &[], now), "no sessions on h\n");
    }

    #[test]
    fn table_of_every_host() {
        let now = 1_000_000;
        let t = render_all(
            &[
                (
                    "devbox",
                    vec![
                        info("main", true, "michel@mbp", 3, now - 7200, "/bin/zsh -l"),
                        info("work", false, "michel@mbp", 60, now - 60, "htop"),
                    ],
                ),
                ("nas", vec![]),
                ("lab-server", vec![info("1", false, "", 0, now, "sh")]),
            ],
            now,
        );
        assert_eq!(
            t,
            "HOST        NAME  STATE     WHO           IDLE  AGE  COMMAND\n\
             devbox      main  attached  michel@mbp    3s    2h   /bin/zsh -l\n\
             devbox      work  detached  (michel@mbp)  1m    1m   htop\n\
             lab-server  1     detached  -             0s    0s   sh\n"
        );
        assert_eq!(render_all(&[("nas", vec![])], now), "");
        assert_eq!(render_all(&[], now), "");
    }
}
