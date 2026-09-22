//! System service integration.
//!
//! Generates and manages a per-project platform service so the trace daemon
//! for that project starts at login and restarts on failure:
//!
//! - Linux: systemd user unit `~/.config/systemd/user/trace-<id>.service`
//! - macOS: launchd agent `~/Library/LaunchAgents/com.trace.daemon.<id>.plist`
//!
//! `<id>` is derived from the project root, so several projects can each have
//! their own service. The service runs `trace daemon --idle-timeout 0 <root>`
//! (never idles out; the service manager owns its lifetime).

use crate::daemon::{paths_for_root, project_id};
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A platform service manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceManager {
    Systemd,
    Launchd,
    Unsupported,
}

fn detect_service_manager() -> ServiceManager {
    if cfg!(target_os = "linux") && Path::new("/run/systemd/system").exists() {
        ServiceManager::Systemd
    } else if cfg!(target_os = "macos") {
        ServiceManager::Launchd
    } else {
        ServiceManager::Unsupported
    }
}

fn short_id(root: &Path) -> String {
    project_id(root)[..12].to_string()
}

fn systemd_unit_name(root: &Path) -> String {
    format!("trace-{}.service", short_id(root))
}

fn launchd_label(root: &Path) -> String {
    format!("com.trace.daemon.{}", short_id(root))
}

/// Quote one ExecStart argument for systemd: double quotes with C escapes,
/// and `%`/`$` doubled so they are not expanded as specifiers/variables.
fn systemd_quote(arg: &str) -> String {
    let mut out = String::from("\"");
    for ch in arg.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Generate the systemd unit file content for the trace daemon.
fn systemd_unit(root: &Path, exe: &Path) -> String {
    let root_str = root.display().to_string();
    format!(
        "[Unit]\n\
Description=trace architectural memory daemon for {desc}\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart={exe} daemon --idle-timeout 0 {root}\n\
WorkingDirectory={workdir}\n\
Restart=on-failure\n\
RestartSec=5\n\
\n\
[Install]\n\
WantedBy=default.target\n",
        desc = root_str.replace('%', "%%"),
        exe = systemd_quote(&exe.display().to_string()),
        root = systemd_quote(&root_str),
        workdir = root_str.replace('%', "%%"),
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Generate the launchd plist content for the trace daemon.
fn launchd_plist(root: &Path, exe: &Path, label: &str, log: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>daemon</string>
        <string>--idle-timeout</string>
        <string>0</string>
        <string>{root}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>WorkingDirectory</key>
    <string>{root}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        label = xml_escape(label),
        exe = xml_escape(&exe.display().to_string()),
        root = xml_escape(&root.display().to_string()),
        log = xml_escape(&log.display().to_string()),
    )
}

fn systemd_unit_path(root: &Path) -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("cannot find the config directory")?
        .join("systemd/user")
        .join(systemd_unit_name(root)))
}

fn launchd_plist_path(root: &Path) -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("cannot find the home directory")?
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", launchd_label(root))))
}

/// Run a command, returning whether it succeeded and its combined output.
fn run(program: &str, args: &[&str]) -> Result<(bool, String)> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    let mut text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !err.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&err);
    }
    Ok((output.status.success(), text))
}

fn systemctl(args: &[&str]) -> Result<()> {
    let mut full = vec!["--user"];
    full.extend_from_slice(args);
    let (ok, out) = run("systemctl", &full)?;
    if ok {
        Ok(())
    } else {
        Err(anyhow!("systemctl --user {} failed: {out}", args.join(" ")))
    }
}

fn current_uid() -> Result<String> {
    let (ok, out) = run("id", &["-u"])?;
    if ok && !out.is_empty() {
        Ok(out)
    } else {
        Err(anyhow!("cannot determine the current user id"))
    }
}

fn resolve_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot determine trace executable path")?;
    Ok(exe.canonicalize().unwrap_or(exe))
}

/// Install (and start) the trace daemon service for `project_root`.
pub fn install(project_root: &Path) -> Result<()> {
    if crate::root::is_broad_root(project_root) {
        return Err(anyhow!(
            "refusing to install a service for {}: pass a project directory (`trace service install /path/to/project`)",
            project_root.display()
        ));
    }
    let exe = resolve_binary()?;
    // Unit files and plists are line-oriented: a newline in a path would
    // inject directives.
    for path in [project_root, exe.as_path()] {
        if path.to_string_lossy().chars().any(char::is_control) {
            return Err(anyhow!(
                "refusing to install a service for a path containing control characters: {:?}",
                path
            ));
        }
    }
    let running = crate::daemon::daemon_status(project_root);
    match detect_service_manager() {
        ServiceManager::Systemd => {
            let unit_path = systemd_unit_path(project_root)?;
            std::fs::create_dir_all(unit_path.parent().unwrap())
                .context("creating the systemd user unit dir")?;
            std::fs::write(&unit_path, systemd_unit(project_root, &exe))
                .context("writing the systemd unit")?;
            systemctl(&["daemon-reload"])?;
            let unit = systemd_unit_name(project_root);
            systemctl(&["enable", &unit])?;
            // restart (not start) so a reinstall after an upgrade runs the new binary.
            systemctl(&["restart", &unit])?;
            println!("✓ {unit} installed and started ({})", unit_path.display());
            println!("  Logs: journalctl --user -u {unit}");
            println!("  To keep it running while logged out: loginctl enable-linger $USER");
        }
        ServiceManager::Launchd => {
            let plist_path = launchd_plist_path(project_root)?;
            std::fs::create_dir_all(plist_path.parent().unwrap())
                .context("creating LaunchAgents")?;
            let label = launchd_label(project_root);
            let log = paths_for_root(project_root).log;
            if let Some(dir) = log.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&plist_path, launchd_plist(project_root, &exe, &label, &log))
                .context("writing the launchd plist")?;
            let uid = current_uid()?;
            let _ = run("launchctl", &["bootout", &format!("gui/{uid}/{label}")]);
            let (ok, out) = run(
                "launchctl",
                &[
                    "bootstrap",
                    &format!("gui/{uid}"),
                    &plist_path.to_string_lossy(),
                ],
            )?;
            if !ok {
                return Err(anyhow!("launchctl bootstrap failed: {out}"));
            }
            println!("✓ {label} installed and started ({})", plist_path.display());
            println!("  Logs: {}", log.display());
        }
        ServiceManager::Unsupported => {
            println!("⚠  No supported service manager found (systemd user session or launchd).");
            println!("  Run the daemon manually, or let `trace serve` start it on demand:");
            println!("    {} daemon {}", exe.display(), project_root.display());
            return Ok(());
        }
    }
    if let (true, Some(pid)) = (running.running, running.pid) {
        println!(
            "  Note: an on-demand daemon (pid {pid}) is serving this project; the service takes over \
             when it exits (after 30 idle minutes), or hand over now with `kill {pid}`."
        );
    }
    Ok(())
}

/// Stop and remove the trace daemon service for `project_root`.
pub fn uninstall(project_root: &Path) -> Result<()> {
    match detect_service_manager() {
        ServiceManager::Systemd => {
            let unit = systemd_unit_name(project_root);
            let unit_path = systemd_unit_path(project_root)?;
            if !unit_path.exists() {
                println!("No trace service installed for {}", project_root.display());
                return Ok(());
            }
            let _ = systemctl(&["disable", "--now", &unit]);
            std::fs::remove_file(&unit_path).context("removing the systemd unit")?;
            let _ = systemctl(&["daemon-reload"]);
            println!("✓ {unit} stopped and removed");
        }
        ServiceManager::Launchd => {
            let plist_path = launchd_plist_path(project_root)?;
            if !plist_path.exists() {
                println!("No trace service installed for {}", project_root.display());
                return Ok(());
            }
            let label = launchd_label(project_root);
            if let Ok(uid) = current_uid() {
                let _ = run("launchctl", &["bootout", &format!("gui/{uid}/{label}")]);
            }
            std::fs::remove_file(&plist_path).context("removing the launchd plist")?;
            println!("✓ {label} stopped and removed");
        }
        ServiceManager::Unsupported => {
            println!("No supported service manager found; nothing to uninstall.");
        }
    }
    Ok(())
}

/// Report whether the project's service is installed and active.
pub fn status(project_root: &Path) -> Result<()> {
    match detect_service_manager() {
        ServiceManager::Systemd => {
            let unit = systemd_unit_name(project_root);
            let installed = systemd_unit_path(project_root)?.exists();
            let (_, state) = run("systemctl", &["--user", "is-active", &unit])?;
            println!(
                "{unit}: {} ({state})",
                if installed {
                    "installed"
                } else {
                    "not installed"
                }
            );
        }
        ServiceManager::Launchd => {
            let label = launchd_label(project_root);
            let installed = launchd_plist_path(project_root)?.exists();
            let loaded = current_uid()
                .and_then(|uid| run("launchctl", &["print", &format!("gui/{uid}/{label}")]))
                .map(|(ok, _)| ok)
                .unwrap_or(false);
            println!(
                "{label}: {}, {}",
                if installed {
                    "installed"
                } else {
                    "not installed"
                },
                if loaded { "loaded" } else { "not loaded" }
            );
        }
        ServiceManager::Unsupported => println!("No supported service manager on this platform."),
    }
    let daemon = crate::daemon::daemon_status(project_root);
    println!(
        "daemon: {}",
        if daemon.running {
            "running"
        } else {
            "not running"
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_quotes_paths_and_escapes_specifiers() {
        let unit = systemd_unit(
            Path::new("/home/me/my project/100%"),
            Path::new("/opt/trace bin/trace"),
        );
        assert!(unit.contains(r#"ExecStart="/opt/trace bin/trace" daemon --idle-timeout 0 "/home/me/my project/100%%""#), "{unit}");
        assert!(unit.contains("WorkingDirectory=/home/me/my project/100%%"));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(!unit.contains("RUST_LOG"));
    }

    #[test]
    fn systemd_quote_escapes() {
        assert_eq!(systemd_quote(r#"a"b\c$d"#), r#""a\"b\\c$$d""#);
    }

    #[test]
    fn plist_escapes_xml() {
        let plist = launchd_plist(
            Path::new("/Users/me/R&D <x>"),
            Path::new("/usr/local/bin/trace"),
            "com.trace.daemon.abc",
            Path::new("/tmp/log"),
        );
        assert!(plist.contains("<string>/Users/me/R&amp;D &lt;x&gt;</string>"));
        assert!(plist.contains("<string>--idle-timeout</string>"));
        assert!(!plist.contains("$(id -u)"));
    }

    #[test]
    fn names_are_per_project() {
        let a = systemd_unit_name(Path::new("/nonexistent/a"));
        let b = systemd_unit_name(Path::new("/nonexistent/b"));
        assert_ne!(a, b);
        assert!(a.starts_with("trace-") && a.ends_with(".service"));
        assert!(launchd_label(Path::new("/nonexistent/a")).starts_with("com.trace.daemon."));
    }

    #[test]
    fn broad_roots_are_refused() {
        assert!(install(Path::new("/")).is_err());
    }
}
