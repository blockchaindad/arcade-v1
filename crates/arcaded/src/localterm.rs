//! Open a terminal on this computer running the arcade CLI.
//!
//! The caller names no program and no arguments. The program is the `arcade` binary beside
//! arcaded (or on PATH), and each terminal is started with that path as one argument. Nothing
//! here is reachable from an API key or from another arcade: the command is Local.

use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Open the terminal. Errors when this computer has no terminal program, or the CLI isn't installed.
pub fn open() -> Result<Value, String> {
    let cli = cli_path()?;
    if !path_ok(&cli) {
        return Err("the arcade CLI's path can't be opened in a terminal".into());
    }
    let mut missing = Vec::new();
    for (bin, args) in launches(&cli) {
        match std::process::Command::new(bin).args(&args).spawn() {
            Ok(mut child) => {
                // A terminal that exits at once didn't open. Try the next one.
                match child.try_wait() {
                    Ok(Some(_)) => missing.push(bin),
                    _ => return Ok(json!({"opened": true})),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => missing.push(bin),
            Err(_) => missing.push(bin),
        }
    }
    Err(format!("no terminal program found ({}); open a terminal and run arcade", missing.join(", ")))
}

fn cli_path() -> Result<PathBuf, String> {
    let name = if cfg!(windows) { "arcade.exe" } else { "arcade" };
    if let Ok(exe) = std::env::current_exe() {
        let beside = exe.with_file_name(name);
        if beside.is_file() {
            return Ok(beside);
        }
    }
    if let Some(p) = on_path(name) {
        return Ok(p);
    }
    Err("the arcade CLI isn't installed next to arcaded, or on PATH".into())
}

fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}

/// Control characters and shell metacharacters stay out, even though the path is one argument:
/// some terminal programs pass the command through a shell of their own.
fn path_ok(p: &Path) -> bool {
    let s = p.as_os_str().to_string_lossy();
    !s.is_empty() && !s.chars().any(|c| c.is_control() || matches!(c, '&' | '|' | ';' | '<' | '>' | '^' | '$' | '`'))
}

/// How each terminal program is asked to run `cli`. The path is always one argument.
fn launches(cli: &Path) -> Vec<(&'static str, Vec<OsString>)> {
    let c = cli.as_os_str();
    let mut v = Vec::new();
    #[cfg(unix)]
    {
        let one = |flag: &str| vec![OsString::from(flag), c.to_os_string()];
        let dash = || vec![OsString::from("--"), c.to_os_string()];
        v.push(("x-terminal-emulator", one("-e")));
        v.push(("gnome-terminal", dash()));
        v.push(("konsole", one("-e")));
        v.push(("xfce4-terminal", one("-e")));
        v.push(("mate-terminal", dash()));
        v.push(("kgx", dash()));
        v.push(("alacritty", one("-e")));
        v.push(("kitty", one("-e")));
        v.push(("wezterm", vec![OsString::from("start"), OsString::from("--"), c.to_os_string()]));
        v.push(("xterm", one("-e")));
    }
    #[cfg(windows)]
    {
        // Windows Terminal takes the program as its own argument. `start` is a cmd builtin, so
        // the title and the program are separate arguments, not one string the caller wrote.
        v.push(("wt", vec![OsString::from("--"), c.to_os_string()]));
        v.push((
            "cmd",
            vec![OsString::from("/c"), OsString::from("start"), OsString::from("Arcade CLI"), OsString::from("cmd"), OsString::from("/k"), c.to_os_string()],
        ));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_terminal_runs_the_cli_and_not_a_shell_command() {
        let cli = Path::new(if cfg!(windows) { r"C:\Arcade\arcade.exe" } else { "/usr/local/bin/arcade" });
        assert!(path_ok(cli));
        assert!(!path_ok(Path::new("/tmp/arcade;rm")));
        assert!(!path_ok(Path::new("/tmp/arcade\n")));
        let all = launches(cli);
        assert!(!all.is_empty());
        for (bin, args) in &all {
            assert!(!matches!(*bin, "sh" | "bash" | "powershell" | "pwsh"), "{bin}");
            assert!(!args.iter().any(|a| a == "-c" || a == "wallet" || a.to_string_lossy().contains("phrase")));
            assert_eq!(args.iter().filter(|a| a.as_os_str() == cli.as_os_str()).count(), 1, "{bin} should be given the CLI once, whole");
        }
    }
}
