//! `acs <host> --list` (DESIGN §4.3): ask the remote proxy for every
//! session's STATUS and print a table.

use std::os::fd::AsRawFd;
use std::process::ExitCode;

use crate::cli::ClientArgs;
use crate::client::{self, code};
use crate::proto::{Decoder, Marker, Msg, StatusInfo};
use crate::ssh::{self, Call};
use crate::sys;

pub fn run(args: &ClientArgs) -> ExitCode {
    let host = args.host_name();
    let remote = ssh::remote_acs(crate::VERSION, &["_proxy", "--list"]);
    let (link, marker) = match client::dial(args, Call::Side, &remote) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("acs: {e}");
            return ExitCode::from(code::UNREACHABLE);
        }
    };
    let rest = match marker {
        Marker::Ready { rest, .. } => rest,
        Marker::Need { .. } => {
            link.close();
            println!(
                "no sessions on {host} (acs {} is not installed there)",
                crate::VERSION
            );
            return ExitCode::SUCCESS;
        }
    };
    let mut dec = Decoder::new();
    dec.push(&rest);
    let mut sessions = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    let from = link.from_fd();
    loop {
        match dec.next_msg() {
            Ok(Some(Msg::StatusReply(s))) => {
                sessions.push(s);
                continue;
            }
            Ok(Some(_)) => continue,
            Ok(None) => {}
            Err(e) => {
                eprintln!("acs: bad reply from {host}: {e}");
                link.close();
                return ExitCode::from(code::ERROR);
            }
        }
        match sys::read(from.as_raw_fd(), &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => dec.push(&buf[..n]),
        }
    }
    link.close();
    print!("{}", render(host, &sessions, sys::unix_now()));
    ExitCode::SUCCESS
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

/// The table `--list` prints.
pub fn render(host: &str, sessions: &[StatusInfo], now: u64) -> String {
    if sessions.is_empty() {
        return format!("no sessions on {host}\n");
    }
    let rows: Vec<[String; 6]> = sessions
        .iter()
        .map(|s| {
            [
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
        })
        .collect();
    let head = ["NAME", "STATE", "WHO", "IDLE", "AGE", "COMMAND"];
    let mut width = head.map(str::len);
    for r in &rows {
        for (w, c) in width.iter_mut().zip(r.iter()) {
            *w = (*w).max(c.chars().count());
        }
    }
    let line = |cells: [&str; 6]| -> String {
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
    let mut out = line(head);
    for r in &rows {
        out.push_str(&line([&r[0], &r[1], &r[2], &r[3], &r[4], &r[5]]));
    }
    out
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
    fn table() {
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
}
