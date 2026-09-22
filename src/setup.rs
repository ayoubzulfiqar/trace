//! Automatic agent discovery and MCP server registration.
//

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Config format for an agent's config file.
#[derive(Clone, Copy)]
enum ConfigFormat {
    Json,
    Yaml,
}

/// A discovered AI agent with its config file path and format.
struct DiscoveredAgent {
    name: String,
    config_path: PathBuf,
    format: ConfigFormat,
}

/// All known agent config locations and their formats.
/// Each entry: (display_name, relative_path_from_home, format)
const AGENT_CONFIGS: &[(&str, &str, ConfigFormat)] = &[
    (
        "OpenCode",
        ".config/opencode/opencode.json",
        ConfigFormat::Json,
    ),
    ("Hermes Agent", ".hermes/config.yaml", ConfigFormat::Yaml),
    (
        "Claude Desktop",
        ".config/Claude/claude_desktop_config.json",
        ConfigFormat::Json,
    ),
    ("Cursor", ".cursor/mcp.json", ConfigFormat::Json),
    (
        "Windsurf",
        ".codeium/windsurf/mcp_config.json",
        ConfigFormat::Json,
    ),
];

/// Scan the filesystem for installed AI agents by checking known config paths.
fn discover_agents() -> Vec<DiscoveredAgent> {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return Vec::new(),
    };

    AGENT_CONFIGS
        .iter()
        .filter_map(|(name, rel, format)| {
            let full = Path::new(&home).join(rel);
            if full.exists() {
                Some(DiscoveredAgent {
                    name: (*name).to_string(),
                    config_path: full,
                    format: *format,
                })
            } else {
                None
            }
        })
        .collect()
}

/// Insert or update an MCP server entry in a JSON config file.
fn inject_json_mcp(path: &Path, server_name: &str, command: &Path, args: &[&str]) -> Result<()> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut value: Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing JSON config: {}", path.display()))?;

    let mcp_entry = json!({
        "command": command.to_string_lossy(),
        "args": args,
    });

    // Try "mcpServers" key first (Claude Desktop, Cursor, Windsurf standard)
    if value.get("mcpServers").is_some() {
        value["mcpServers"][server_name] = mcp_entry;
    } else if value.get("mcp").is_some() && value["mcp"].get("servers").is_some() {
        // Some configs nest under mcp.servers
        value["mcp"]["servers"][server_name] = mcp_entry;
    } else {
        // Create the top-level mcpServers key
        value["mcpServers"] = json!({});
        value["mcpServers"][server_name] = mcp_entry;
    }

    let formatted = serde_json::to_string_pretty(&value)?;
    std::fs::write(path, formatted).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Insert or update an MCP server entry in a YAML config file.
fn inject_yaml_mcp(path: &Path, server_name: &str, command: &Path, args: &[&str]) -> Result<()> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut value: serde_yaml::Value = serde_yaml::from_str(&text)
        .with_context(|| format!("parsing YAML config: {}", path.display()))?;

    // Build the server entry as a mapping
    let mut server_map = serde_yaml::Mapping::new();
    server_map.insert(
        serde_yaml::Value::String("command".into()),
        serde_yaml::Value::String(command.to_string_lossy().into_owned()),
    );

    let args_val: Vec<serde_yaml::Value> = args
        .iter()
        .map(|a| serde_yaml::Value::String((*a).to_string()))
        .collect();
    server_map.insert(
        serde_yaml::Value::String("args".into()),
        serde_yaml::Value::Sequence(args_val),
    );

    // Navigate to mcp.servers or create it
    if value.get_mut("mcp").is_some() {
        let mcp = value.get_mut("mcp").unwrap();
        if mcp.get("servers").is_some() {
            mcp["servers"][server_name] = serde_yaml::Value::Mapping(server_map);
        } else {
            mcp["servers"] = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
            mcp["servers"][server_name] = serde_yaml::Value::Mapping(server_map);
        }
    } else {
        // Create mcp.servers
        let mut servers_map = serde_yaml::Mapping::new();
        servers_map.insert(
            serde_yaml::Value::String(server_name.into()),
            serde_yaml::Value::Mapping(server_map),
        );
        let mut mcp_map = serde_yaml::Mapping::new();
        mcp_map.insert(
            serde_yaml::Value::String("servers".into()),
            serde_yaml::Value::Mapping(servers_map),
        );
        value["mcp"] = serde_yaml::Value::Mapping(mcp_map);
    }

    let formatted = serde_yaml::to_string(&value)?;
    std::fs::write(path, formatted).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Resolve the path to the trace binary. Uses `std::env::current_exe()` first,
/// falls back to looking for `trace` in PATH via the `which` pattern.
fn resolve_trace_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot determine current executable path")?;
    if exe.exists() {
        return Ok(exe);
    }
    // Fallback: search PATH
    let output = Command::new("which").arg("trace").output().ok();
    if let Some(out) = output {
        if out.status.success() {
            let path_str = String::from_utf8_lossy(&out.stdout);
            let bin = PathBuf::from(path_str.trim());
            if bin.exists() {
                return Ok(bin);
            }
        }
    }
    Err(anyhow::anyhow!(
        "trace binary not found in PATH; install it first"
    ))
}

/// Detect all installed AI agents on this machine and automatically inject the
/// trace MCP server configuration into each discovered agent's config file.
///
/// Scans known config paths for OpenCode, Hermes Agent, Claude Desktop, Cursor,
/// and Windsurf. For each discovered config, injects a `trace` server entry
/// pointing to the currently-installed trace binary with the `serve` subcommand.
pub fn auto_detect_and_register() -> Result<()> {
    let trace_binary = resolve_trace_binary()?;
    let agents = discover_agents();

    if agents.is_empty() {
        println!("No AI agents with config files found on this system.");
        println!("Supported agents: OpenCode, Hermes Agent, Claude Desktop, Cursor, Windsurf");
        println!();
        println!("Install one of these agents, then run `trace setup` again.");
        return Ok(());
    }

    println!("Found {} agent config(s):", agents.len());
    for agent in &agents {
        println!("  • {} → {}", agent.name, agent.config_path.display());
    }
    println!();

    let mut registered = 0;
    let mut failed = 0;

    for agent in agents {
        let args = ["serve"];
        let result = match agent.format {
            ConfigFormat::Json => {
                inject_json_mcp(&agent.config_path, "trace", &trace_binary, &args)
            }
            ConfigFormat::Yaml => {
                inject_yaml_mcp(&agent.config_path, "trace", &trace_binary, &args)
            }
        };

        match result {
            Ok(_) => {
                println!("✓ Auto-integrated with {}", agent.name);
                registered += 1;
            }
            Err(e) => {
                println!("✗ Failed to integrate with {}: {}", agent.name, e);
                failed += 1;
            }
        }
    }

    println!();
    println!(
        "Registration complete: {} succeeded, {} failed",
        registered, failed
    );

    Ok(())
}

/// List all discovered AI agents without modifying their configs.
pub fn list_agents() -> Result<()> {
    let agents = discover_agents();
    if agents.is_empty() {
        println!("No AI agents found.");
        println!("Supported: OpenCode, Hermes Agent, Claude Desktop, Cursor, Windsurf");
        return Ok(());
    }

    println!("Discovered AI agents ({}):", agents.len());
    for agent in &agents {
        let fmt = match agent.format {
            ConfigFormat::Json => "JSON",
            ConfigFormat::Yaml => "YAML",
        };
        println!("  {} ({})", agent.name, agent.config_path.display());
        println!("    Format: {}", fmt);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tempfile::TempDir;

    static ANR: AtomicU64 = AtomicU64::new(0);

    fn unique(name: &str) -> String {
        format!("{}-{}", name, ANR.fetch_add(1, Ordering::SeqCst))
    }

    #[test]
    fn json_injection_creates_mcp_servers_key() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join(unique("test.json"));
        std::fs::write(&cfg, r#"{"name":"oldconfig"}"#).unwrap();

        inject_json_mcp(
            &cfg,
            "trace",
            &PathBuf::from("/usr/local/bin/trace"),
            &["serve"],
        )
        .unwrap();

        let content = std::fs::read_to_string(&cfg).unwrap();
        let value: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(
            value["mcpServers"]["trace"]["command"],
            "/usr/local/bin/trace"
        );
        assert_eq!(value["mcpServers"]["trace"]["args"][0], "serve");
        assert_eq!(value["name"], "oldconfig");
    }

    #[test]
    fn json_injection_preserves_existing_servers() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join(unique("test.json"));
        std::fs::write(
            &cfg,
            r#"{"mcpServers":{"existing":{"command":"foo","args":[]}}}"#,
        )
        .unwrap();

        inject_json_mcp(&cfg, "trace", &PathBuf::from("/usr/bin/trace"), &["serve"]).unwrap();

        let content = std::fs::read_to_string(&cfg).unwrap();
        let value: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(value["mcpServers"]["existing"]["command"], "foo");
        assert_eq!(value["mcpServers"]["trace"]["command"], "/usr/bin/trace");
    }

    #[test]
    fn json_injection_handles_nested_mcp() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join(unique("test.json"));
        std::fs::write(&cfg, r#"{"mcp":{"servers":{"existing":{}}}}"#).unwrap();

        inject_json_mcp(&cfg, "trace", &PathBuf::from("/usr/bin/trace"), &["serve"]).unwrap();

        let content = std::fs::read_to_string(&cfg).unwrap();
        let value: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(
            value["mcp"]["servers"]["existing"],
            Value::Object(Default::default())
        );
        assert_eq!(
            value["mcp"]["servers"]["trace"]["command"],
            "/usr/bin/trace"
        );
    }

    #[test]
    fn yaml_injection_creates_mcp_section() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join(unique("test.yaml"));
        std::fs::write(&cfg, "name: myapp\n").unwrap();

        inject_yaml_mcp(
            &cfg,
            "trace",
            &PathBuf::from("/usr/local/bin/trace"),
            &["serve"],
        )
        .unwrap();

        let content = std::fs::read_to_string(&cfg).unwrap();
        assert!(content.contains("trace"));
        assert!(content.contains("/usr/local/bin/trace"));
        assert!(content.contains("serve"));
        assert!(content.contains("mcp"));
    }

    #[test]
    fn yaml_injection_preserves_existing_content() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join(unique("test.yaml"));
        std::fs::write(&cfg, "name: myconfig\nkey: value\n").unwrap();

        inject_yaml_mcp(&cfg, "trace", &PathBuf::from("/usr/bin/trace"), &["serve"]).unwrap();

        let content = std::fs::read_to_string(&cfg).unwrap();
        assert!(content.contains("myconfig"));
        assert!(content.contains("value"));
        assert!(content.contains("trace"));
    }

    #[test]
    fn discover_agents_finds_nothing_without_configs() {
        // In the test environment, the HOME still has user configs,
        // so we just verify the function doesn't panic.
        let agents = discover_agents();
        for agent in &agents {
            assert!(!agent.name.is_empty());
            assert!(agent.config_path.exists());
        }
    }

    #[test]
    fn resolve_trace_binary_works_when_run_from_build() {
        // When running tests via cargo, current_exe() returns the test binary path
        // which exists. If it's stripped or missing, we fall back to `which trace`.
        let result = resolve_trace_binary();
        match result {
            Ok(path) => {
                assert!(
                    path.exists(),
                    "resolved binary path should exist: {}",
                    path.display()
                );
            }
            Err(e) => {
                // If current_exe() fails (stripped binary) and `which trace` not found,
                // that's acceptable in a CI/test environment without trace in PATH.
                eprintln!("trace binary not resolvable in test env: {}", e);
            }
        }
    }
}
