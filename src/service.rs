//! System service integration.
//!
//! Generates and manages platform-native service definitions so the trace
//! daemon can run as a persistent background process:
//!
//! - Linux: systemd user service (`~/.config/systemd/user/trace.service`)
//! - macOS: launchd plist (`~/Library/LaunchAgents/com.trace.daemon.plist`)
//!
//! The service runs `trace daemon <project-root>` — a long-lived Unix socket
//! listener that all `trace serve` shims connect to, avoiding duplicated
//! per-agent process overhead.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A platform service manager.
enum ServiceManager {
    Systemd,
    #[cfg(target_os = "macos")]
    Launchd,
    Unsupported,
}

fn detect_service_manager() -> ServiceManager {
    #[cfg(target_os = "linux")]
    {
        if Path::new("/run/systemd/system").exists() {
            return ServiceManager::Systemd;
        }
    }
    #[cfg(target_os = "macos")]
    {
        if Path::new("/System/Library/LaunchDaemons").exists() {
            return ServiceManager::Launchd;
        }
    }
    ServiceManager::Unsupported
}

/// Generate the systemd unit file content for the trace daemon.
fn systemd_unit(root: &Path, exe: &Path) -> String {
    format!(
        "[Unit]\n\
Description=Trace Architectural Memory Engine Daemon\n\
After=network.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart={} daemon {}\n\
WorkingDirectory={}\n\
Restart=on-failure\n\
RestartSec=5\n\
Environment=RUST_LOG=info\n\
\n\
[Install]\n\
WantedBy=default.target\n",
        exe.display(),
        root.display(),
        root.display()
    )
}

/// Generate the launchd plist content for the trace daemon.
#[cfg(target_os = "macos")]
fn launchd_plist(root: &Path, exe: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.trace.daemon</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>daemon</string>
        <string>{root}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>WorkingDirectory</key>
    <string>{root}</string>
</dict>
</plist>
"#,
        exe = exe.display(),
        root = root.display()
    )
}

/// Resolve the trace binary path.
fn resolve_binary() -> Result<PathBuf> {
    std::env::current_exe().context("cannot determine trace executable path")
}

/// Install the trace daemon as a system service.
pub fn install(project_root: &Path) -> Result<()> {
    let exe = resolve_binary()?;
    let manager = detect_service_manager();

    match manager {
        ServiceManager::Systemd => {
            install_systemd(project_root, &exe)?;
            println!("trace daemon registered with systemd");
        }
        #[cfg(target_os = "macos")]
        ServiceManager::Launchd => {
            install_launchd(project_root, &exe)?;
            println!("trace daemon registered with launchd");
        }
        ServiceManager::Unsupported => {
            println!("⚠  No supported service manager found (systemd or launchd).");
            println!("  The trace daemon can still be run manually:");
            println!("    {} daemon {}", exe.display(), project_root.display());
            println!("  Or started from a terminal multiplexer (tmux/screen) for persistence.");
        }
    }

    Ok(())
}

fn install_systemd(root: &Path, exe: &Path) -> Result<()> {
    let unit = systemd_unit(root, exe);
    let config_dir = dirs::config_dir()
        .context("cannot find config directory")?
        .join("systemd/user");
    std::fs::create_dir_all(&config_dir).context("creating systemd user config dir")?;

    let unit_path = config_dir.join("trace.service");
    std::fs::write(&unit_path, unit).context("writing systemd unit file")?;

    // Reload systemd user daemon
    let _ = Command::new("systemctl")
        .args(&["--user", "daemon-reload"])
        .status()
        .context("reloading systemd user daemon");

    Ok(())
}

#[cfg(target_os = "macos")]
fn install_launchd(root: &Path, exe: &Path) -> Result<()> {
    let plist = launchd_plist(root, exe);
    let agents_dir = dirs::home_dir()
        .context("cannot find home directory")?
        .join("Library/LaunchAgents");
    std::fs::create_dir_all(&agents_dir).context("creating LaunchAgents dir")?;

    let plist_path = agents_dir.join("com.trace.daemon.plist");
    std::fs::write(&plist_path, plist).context("writing launchd plist")?;

    Ok(())
}

/// Remove the trace daemon service definition.
pub fn uninstall() -> Result<()> {
    let manager = detect_service_manager();

    match manager {
        ServiceManager::Systemd => {
            let unit_path = dirs::config_dir()
                .context("cannot find config directory")?
                .join("systemd/user/trace.service");
            if unit_path.exists() {
                std::fs::remove_file(&unit_path).context("removing systemd unit file")?;
                let _ = Command::new("systemctl")
                    .args(&["--user", "daemon-reload"])
                    .status();
                println!("✓ trace service removed from systemd");
            } else {
                println!("trace service not found in systemd configuration");
            }
        }
        #[cfg(target_os = "macos")]
        ServiceManager::Launchd => {
            let plist_path = dirs::home_dir()
                .context("cannot find home directory")?
                .join("Library/LaunchAgents/com.trace.daemon.plist");
            if plist_path.exists() {
                let _ = Command::new("launchctl")
                    .args(&[
                        "bootout",
                        "gui/$(id -u)",
                        &plist_path.to_string_lossy().to_string(),
                    ])
                    .status();
                std::fs::remove_file(&plist_path).context("removing launchd plist")?;
                println!("✓ trace service removed from launchd");
            } else {
                println!("trace service not found in launchd configuration");
            }
        }
        ServiceManager::Unsupported => {
            println!("No supported service manager found; nothing to uninstall.");
        }
    }

    Ok(())
}
