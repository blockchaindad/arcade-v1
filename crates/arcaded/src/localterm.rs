//! Open a terminal on this computer with the arcade CLI on its PATH.
//!
//! The caller names no program and no arguments. A terminal emulator is started with a shell
//! (bash, or cmd on Windows) and the directory that holds `arcade` prepended to PATH, so the
//! window stays open and `arcade` works. Running the CLI itself as the terminal's command prints
//! help and exits. Nothing here is reachable from an API key or from another arcade.

use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Child;

/// Open the terminal. Errors when this computer has no terminal program, or the CLI isn't installed.
pub fn open() -> Result<Value, String> {
    let cli = cli_path()?;
    if !path_ok(&cli) {
        return Err("the arcade CLI's path can't be opened in a terminal".into());
    }
    let dir = cli.parent().ok_or("the arcade CLI's path can't be opened in a terminal")?;
    let path = path_with_dir(dir);
    let mut missing = Vec::new();
    for (bin, args) in shell_launches() {
        match std::process::Command::new(bin).args(&args).env("PATH", &path).spawn() {
            Ok(mut child) => {
                // A terminal that exits at once didn't open. Try the next one.
                match child.try_wait() {
                    Ok(Some(_)) => missing.push(bin),
                    _ => {
                        reap(child);
                        return Ok(json!({"opened": true}));
                    }
                }
            }
            Err(_) => missing.push(bin),
        }
    }
    Err(format!("no terminal program found ({}); open a terminal and run arcade", missing.join(", ")))
}

/// Wait for the terminal in the background so it is reaped when the user closes it.
fn reap(mut child: Child) {
    let _ = std::thread::Builder::new().name("terminal".into()).spawn(move || {
        let _ = child.wait();
    });
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

/// The CLI's directory first, then the PATH this process already has.
fn path_with_dir(dir: &Path) -> OsString {
    let mut p = dir.as_os_str().to_os_string();
    if let Some(old) = std::env::var_os("PATH") {
        p.push(if cfg!(windows) { ";" } else { ":" });
        p.push(old);
    }
    p
}

/// Control characters and shell metacharacters stay out of the directory we put on PATH.
fn path_ok(p: &Path) -> bool {
    let s = p.as_os_str().to_string_lossy();
    !s.is_empty() && !s.chars().any(|c| c.is_control() || matches!(c, '&' | '|' | ';' | '<' | '>' | '^' | '$' | '`'))
}

/// A shell that stays open. The CLI is not the command: that prints help and the window closes.
fn shell_launches() -> Vec<(&'static str, Vec<OsString>)> {
    let mut v = Vec::new();
    #[cfg(unix)]
    {
        let bash = |flag: &str| vec![OsString::from(flag), OsString::from("bash")];
        let dash = || vec![OsString::from("--"), OsString::from("bash")];
        v.push(("x-terminal-emulator", bash("-e")));
        v.push(("gnome-terminal", dash()));
        v.push(("konsole", bash("-e")));
        v.push(("xfce4-terminal", bash("-e")));
        v.push(("mate-terminal", dash()));
        v.push(("kgx", dash()));
        v.push(("alacritty", bash("-e")));
        v.push(("kitty", bash("-e")));
        v.push(("wezterm", vec![OsString::from("start"), OsString::from("--"), OsString::from("bash")]));
        v.push(("xterm", bash("-e")));
    }
    #[cfg(windows)]
    {
        // Windows Terminal, then a console that stays open (`cmd /k`). No command string.
        v.push(("wt", vec![OsString::from("--"), OsString::from("cmd")]));
        v.push((
            "cmd",
            vec![OsString::from("/c"), OsString::from("start"), OsString::from("Arcade CLI"), OsString::from("cmd"), OsString::from("/k")],
        ));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_terminal_opens_a_shell_with_the_cli_on_path() {
        let cli = Path::new(if cfg!(windows) { r"C:\Arcade\arcade.exe" } else { "/usr/local/bin/arcade" });
        assert!(path_ok(cli));
        assert!(!path_ok(Path::new("/tmp/arcade;rm")));
        assert!(!path_ok(Path::new("/tmp/arcade\n")));
        let path = path_with_dir(cli.parent().unwrap());
        let shown = path.to_string_lossy();
        assert!(shown.starts_with(cli.parent().unwrap().to_string_lossy().as_ref()));
        let all = shell_launches();
        assert!(!all.is_empty());
        for (bin, args) in &all {
            assert!(!matches!(*bin, "sh" | "bash" | "powershell" | "pwsh"), "{bin} is a terminal, not the shell itself");
            assert!(!args.iter().any(|a| a == "-c" || a == "wallet" || a.to_string_lossy().contains("phrase")), "{bin}");
            assert!(args.iter().any(|a| a == "bash" || a == "sh" || a == "cmd"), "{bin} should open a shell");
            assert!(!args.iter().any(|a| a.as_os_str() == cli.as_os_str()), "{bin} must not run the CLI (it would print help and exit)");
        }
    }
}
