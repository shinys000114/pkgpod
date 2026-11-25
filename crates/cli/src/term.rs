// SPDX-License-Identifier: AGPL-3.0-only

//! Terminal helpers: passwords without echo, confirmations.

use std::io::{self, BufRead, Write};

use rustix::termios::{LocalModes, OptionalActions, tcgetattr, tcsetattr};

/// Prompts on the controlling terminal and reads a line without echo.
pub fn read_password(prompt: &str) -> io::Result<String> {
    // In a script or a CI job there is none to ask on.
    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|e| {
            io::Error::new(e.kind(), format!("no terminal ({e}): use --password-stdin"))
        })?;
    let old = tcgetattr(&tty)?;
    let mut quiet = old.clone();
    quiet.local_modes.remove(LocalModes::ECHO);
    quiet.local_modes.insert(LocalModes::ECHONL);
    tcsetattr(&tty, OptionalActions::Flush, &quiet)?;
    let mut line = String::new();
    let read = (&tty)
        .write_all(prompt.as_bytes())
        .and_then(|()| io::BufReader::new(&tty).read_line(&mut line));
    let _ = tcsetattr(&tty, OptionalActions::Flush, &old);
    read?;
    Ok(trim_newline(line))
}

/// Reads one line from stdin (for `--password-stdin`).
pub fn read_stdin_line() -> io::Result<String> {
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(trim_newline(line))
}

/// Asks a yes/no question on the terminal; false without one.
pub fn confirm(question: &str) -> bool {
    let Ok(tty) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    else {
        return false;
    };
    let mut line = String::new();
    if (&tty)
        .write_all(format!("{question} [y/N] ").as_bytes())
        .and_then(|()| io::BufReader::new(&tty).read_line(&mut line))
        .is_err()
    {
        return false;
    }
    matches!(line.trim(), "y" | "Y" | "yes")
}

fn trim_newline(mut s: String) -> String {
    while s.ends_with(['\n', '\r']) {
        s.pop();
    }
    s
}
