//! A debug log for Control Room: when signalbox runs under controlroom
//! (`$CONTROLROOM_LOG` set), its TUIs append what they do there (keys, status
//! changes), so an agent testing signalbox can follow it with `controlroom logs`.

use std::io::Write;

pub fn log(who: &str, msg: impl AsRef<str>) {
    let Some(path) = std::env::var_os("CONTROLROOM_LOG") else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{} {who}: {}", clock(), msg.as_ref());
    }
}

/// HH:MM:SS (UTC).
fn clock() -> String {
    let s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}
