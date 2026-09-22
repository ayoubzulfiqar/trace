//! Automatic agent discovery and MCP server registration.
//!
//! Every supported agent is described by where its config lives and how an
//! MCP server entry is shaped there. Edits are idempotent, atomic
//! (write-to-temp + rename, permissions preserved, symlinks followed), keep a
//! `.trace-backup` of the previous file, and preserve formatting where the
//! format allows it: TOML through `toml_edit`, YAML by text insertion (so
//! comments survive), JSON with key order preserved.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// Name of the server entry written into agent configs.
pub const SERVER_NAME: &str = "trace";

/// How an agent's config file is structured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    /// JSON object at `key` holding `{name: entry}`.
    Json {
        key: &'static str,
        style: EntryStyle,
    },
    /// Codex CLI: `[mcp_servers.<name>]` in TOML.
    CodexToml,
    /// Hermes Agent: `mcp_servers.<name>` in YAML.
    HermesYaml,
}

/// Shape of a JSON server entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryStyle {
    /// `{"command": ..., "args": [...]}` (Claude Desktop, Cursor, Windsurf, Gemini).
    Standard,
    /// `{"type": "stdio", "command": ..., "args": [...], "env": {}}` (Claude Code).
    ClaudeCode,
    /// `{"type": "stdio", "command": ..., "args": [...]}` (VS Code `mcp.json`).
    VsCode,
    /// `{"type": "local", "command": [cmd, ...args], "enabled": true}` (OpenCode).
    OpenCode,
}

struct AgentSpec {
    name: &'static str,
    /// Config file location.
    config: fn() -> Option<PathBuf>,
    /// A directory whose presence means the agent is installed even if the
    /// config file does not exist yet (the file is then created).
    install_dir: fn() -> Option<PathBuf>,
    format: Format,
    /// Launches servers from the project directory (so `trace serve` can
    /// discover the root). Global-only clients need `--root`.
    project_aware: bool,
}

fn home() -> Option<PathBuf> {
    dirs::home_dir()
}

fn xdg_config() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".config")))
}

fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".codex")))
}

const AGENTS: &[AgentSpec] = &[
    AgentSpec {
        name: "Claude Code",
        config: || home().map(|h| h.join(".claude.json")),
        install_dir: || None,
        format: Format::Json {
            key: "mcpServers",
            style: EntryStyle::ClaudeCode,
        },
        project_aware: true,
    },
    AgentSpec {
        name: "Claude Desktop",
        config: || dirs::config_dir().map(|c| c.join("Claude/claude_desktop_config.json")),
        install_dir: || dirs::config_dir().map(|c| c.join("Claude")),
        format: Format::Json {
            key: "mcpServers",
            style: EntryStyle::Standard,
        },
        project_aware: false,
    },
    AgentSpec {
        name: "Cursor",
        config: || home().map(|h| h.join(".cursor/mcp.json")),
        install_dir: || home().map(|h| h.join(".cursor")),
        format: Format::Json {
            key: "mcpServers",
            style: EntryStyle::Standard,
        },
        project_aware: true,
    },
    AgentSpec {
        name: "Windsurf",
        config: || home().map(|h| h.join(".codeium/windsurf/mcp_config.json")),
        install_dir: || home().map(|h| h.join(".codeium/windsurf")),
        format: Format::Json {
            key: "mcpServers",
            style: EntryStyle::Standard,
        },
        project_aware: false,
    },
    AgentSpec {
        name: "Gemini CLI",
        config: || home().map(|h| h.join(".gemini/settings.json")),
        install_dir: || home().map(|h| h.join(".gemini")),
        format: Format::Json {
            key: "mcpServers",
            style: EntryStyle::Standard,
        },
        project_aware: true,
    },
    AgentSpec {
        name: "VS Code",
        config: || dirs::config_dir().map(|c| c.join("Code/User/mcp.json")),
        install_dir: || None,
        format: Format::Json {
            key: "servers",
            style: EntryStyle::VsCode,
        },
        project_aware: true,
    },
    AgentSpec {
        name: "OpenCode",
        config: || xdg_config().map(|c| c.join("opencode/opencode.json")),
        install_dir: || xdg_config().map(|c| c.join("opencode")),
        format: Format::Json {
            key: "mcp",
            style: EntryStyle::OpenCode,
        },
        project_aware: true,
    },
    AgentSpec {
        name: "Codex CLI",
        config: || codex_home().map(|c| c.join("config.toml")),
        install_dir: codex_home,
        format: Format::CodexToml,
        project_aware: true,
    },
    AgentSpec {
        name: "Hermes Agent",
        config: || home().map(|h| h.join(".hermes/config.yaml")),
        install_dir: || None,
        format: Format::HermesYaml,
        project_aware: true,
    },
];

/// A discovered AI agent with its config file path and format.
struct DiscoveredAgent {
    spec: &'static AgentSpec,
    config_path: PathBuf,
    exists: bool,
}

/// Scan the filesystem for installed AI agents by checking known config paths.
fn discover_agents() -> Vec<DiscoveredAgent> {
    AGENTS
        .iter()
        .filter_map(|spec| {
            let config_path = (spec.config)()?;
            let exists = config_path.is_file();
            let installed = exists || (spec.install_dir)().is_some_and(|d| d.is_dir());
            installed.then_some(DiscoveredAgent {
                spec,
                config_path,
                exists,
            })
        })
        .collect()
}

// ── Entry construction ─────────────────────────────────────────────────────────

fn json_entry(style: EntryStyle, command: &str, args: &[String]) -> Value {
    match style {
        EntryStyle::Standard => json!({ "command": command, "args": args }),
        EntryStyle::ClaudeCode => {
            json!({ "type": "stdio", "command": command, "args": args, "env": {} })
        }
        EntryStyle::VsCode => json!({ "type": "stdio", "command": command, "args": args }),
        EntryStyle::OpenCode => {
            let mut cmd = vec![command.to_string()];
            cmd.extend(args.iter().cloned());
            json!({ "type": "local", "command": cmd, "enabled": true })
        }
    }
}

/// The server entry setup wants in place.
#[derive(Debug, Clone, Copy)]
struct Desired<'a> {
    command: &'a str,
    args: &'a [String],
    /// `--root` was given: args are replaced even if the user customised them.
    args_explicit: bool,
}

/// Merge the desired command into an existing entry, keeping the user's
/// other settings (`env`, timeouts, `enabled: false`, a pinned root in
/// `args`…). Only the binary path is always refreshed.
fn merge_json_entry(existing: Option<&Value>, style: EntryStyle, d: &Desired) -> Value {
    let fresh = json_entry(style, d.command, d.args);
    let Some(Value::Object(old)) = existing else {
        return fresh;
    };
    let mut merged = old.clone();
    match style {
        EntryStyle::OpenCode => {
            let command = match merged.get("command").and_then(Value::as_array) {
                Some(arr) if !d.args_explicit && !arr.is_empty() => {
                    let mut arr = arr.clone();
                    arr[0] = json!(d.command);
                    Value::Array(arr)
                }
                _ => fresh["command"].clone(),
            };
            merged.insert("command".into(), command);
            merged.entry("type").or_insert_with(|| json!("local"));
            merged.entry("enabled").or_insert_with(|| json!(true));
        }
        _ => {
            merged.insert("command".into(), json!(d.command));
            let has_args = merged.get("args").is_some_and(Value::is_array);
            if d.args_explicit || !has_args {
                merged.insert("args".into(), json!(d.args));
            }
            if matches!(style, EntryStyle::ClaudeCode | EntryStyle::VsCode) {
                merged.entry("type").or_insert_with(|| json!("stdio"));
            }
        }
    }
    Value::Object(merged)
}

/// Result of editing one config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Added,
    Updated,
    Unchanged,
    Removed,
    NotPresent,
}

impl Change {
    fn describe(self, dry_run: bool) -> &'static str {
        match (self, dry_run) {
            (Change::Added, false) => "added",
            (Change::Added, true) => "would add",
            (Change::Updated, false) => "updated",
            (Change::Updated, true) => "would update",
            (Change::Removed, false) => "removed",
            (Change::Removed, true) => "would remove",
            (Change::Unchanged, _) => "already configured",
            (Change::NotPresent, _) => "not configured",
        }
    }

    fn writes(self) -> bool {
        matches!(self, Change::Added | Change::Updated | Change::Removed)
    }
}

// ── JSON ───────────────────────────────────────────────────────────────────────

fn parse_json_config(text: &str) -> Result<Value> {
    if text.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let value: Value = serde_json::from_str(text)
        .context("not plain JSON (comments or trailing commas?) — add the entry manually")?;
    if !value.is_object() {
        return Err(anyhow!("top level is not a JSON object"));
    }
    Ok(value)
}

fn upsert_json(root: &mut Value, key: &str, name: &str, entry: Value) -> Result<Change> {
    let obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("top level is not a JSON object"))?;
    let servers = obj
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| anyhow!("\"{key}\" exists but is not an object"))?;
    Ok(match servers.get(name) {
        Some(existing) if *existing == entry => Change::Unchanged,
        Some(_) => {
            servers.insert(name.to_string(), entry);
            Change::Updated
        }
        None => {
            servers.insert(name.to_string(), entry);
            Change::Added
        }
    })
}

fn remove_json(root: &mut Value, key: &str, name: &str) -> Change {
    match root
        .get_mut(key)
        .and_then(Value::as_object_mut)
        .and_then(|servers| servers.shift_remove(name))
    {
        Some(_) => Change::Removed,
        None => Change::NotPresent,
    }
}

fn json_configured(root: &Value, key: &str, name: &str) -> bool {
    root.get(key).and_then(|s| s.get(name)).is_some()
}

fn render_json(value: &Value) -> Result<String> {
    let mut out = serde_json::to_string_pretty(value)?;
    out.push('\n');
    Ok(out)
}

// ── TOML (Codex) ───────────────────────────────────────────────────────────────

fn edit_codex_toml(text: &str, name: &str, desired: Option<&Desired>) -> Result<(String, Change)> {
    let mut doc: toml_edit::DocumentMut = text.parse().context("invalid TOML")?;
    let Some(d) = desired else {
        let removed = doc
            .get_mut("mcp_servers")
            .and_then(|s| s.as_table_like_mut())
            .and_then(|t| t.remove(name))
            .is_some();
        return Ok((
            doc.to_string(),
            if removed {
                Change::Removed
            } else {
                Change::NotPresent
            },
        ));
    };
    let servers = doc
        .entry("mcp_servers")
        .or_insert_with(|| {
            let mut t = toml_edit::Table::new();
            t.set_implicit(true);
            toml_edit::Item::Table(t)
        })
        .as_table_like_mut()
        .ok_or_else(|| anyhow!("mcp_servers is not a table"))?;
    let args_array = || {
        let mut arr = toml_edit::Array::new();
        for a in d.args {
            arr.push(a.as_str());
        }
        toml_edit::value(arr)
    };
    let Some(existing) = servers
        .get_mut(name)
        .and_then(|item| item.as_table_like_mut())
    else {
        let mut table = toml_edit::Table::new();
        table.insert("command", toml_edit::value(d.command));
        table.insert("args", args_array());
        servers.insert(name, toml_edit::Item::Table(table));
        return Ok((doc.to_string(), Change::Added));
    };
    // Keep the user's other keys (env, timeouts, a pinned root in args).
    let current_args: Option<Vec<String>> =
        existing.get("args").and_then(|v| v.as_array()).map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        });
    let replace_args =
        (d.args_explicit || current_args.is_none()) && current_args.as_deref() != Some(d.args);
    let replace_command = existing.get("command").and_then(|v| v.as_str()) != Some(d.command);
    if !replace_args && !replace_command {
        return Ok((text.to_string(), Change::Unchanged));
    }
    if replace_command {
        existing.insert("command", toml_edit::value(d.command));
    }
    if replace_args {
        existing.insert("args", args_array());
    }
    Ok((doc.to_string(), Change::Updated))
}

fn codex_configured(text: &str, name: &str) -> bool {
    text.parse::<toml_edit::DocumentMut>()
        .ok()
        .and_then(|d| {
            d.get("mcp_servers")
                .and_then(|s| s.as_table_like())
                .map(|t| t.contains_key(name))
        })
        .unwrap_or(false)
}

// ── YAML (Hermes) ──────────────────────────────────────────────────────────────

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn is_content(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && !t.starts_with('#')
}

/// The desired entry, merged into the existing one when there is one.
fn yaml_desired(existing: Option<&serde_yaml::Value>, d: &Desired) -> serde_yaml::Value {
    let args = || serde_yaml::Value::Sequence(d.args.iter().map(|a| a.as_str().into()).collect());
    let mut map = match existing {
        Some(serde_yaml::Value::Mapping(old)) => old.clone(),
        _ => serde_yaml::Mapping::new(),
    };
    map.insert("command".into(), d.command.into());
    let has_args = map.get("args").is_some_and(serde_yaml::Value::is_sequence);
    if d.args_explicit || !has_args {
        map.insert("args".into(), args());
    }
    serde_yaml::Value::Mapping(map)
}

/// `name:` followed by `entry` as an indented block mapping.
fn yaml_entry_lines(name: &str, entry: &serde_yaml::Value, indent: usize) -> Result<Vec<String>> {
    let pad = " ".repeat(indent);
    let body = serde_yaml::to_string(entry)?;
    let mut lines = vec![format!("{pad}{name}:")];
    lines.extend(body.lines().map(|l| format!("{pad}{pad}{l}")));
    Ok(lines)
}

/// Line range `[start, end)` of `name:` directly under the `mcp_servers:`
/// block that starts at `block`, plus the block's child indent.
fn yaml_entry_range(lines: &[&str], block: usize, name: &str) -> (usize, Option<(usize, usize)>) {
    let child_indent = lines[block + 1..]
        .iter()
        .find(|l| is_content(l))
        .map(|l| indent_of(l))
        .filter(|i| *i > 0)
        .unwrap_or(2);
    let block_end = lines[block + 1..]
        .iter()
        .position(|l| is_content(l) && indent_of(l) == 0)
        .map(|i| block + 1 + i)
        .unwrap_or(lines.len());
    let key = format!("{name}:");
    let start = (block + 1..block_end).find(|&i| {
        let l = lines[i];
        indent_of(l) == child_indent && l.trim_start().starts_with(&key)
    });
    let range = start.map(|s| {
        let end = (s + 1..block_end)
            .find(|&i| is_content(lines[i]) && indent_of(lines[i]) <= child_indent)
            .unwrap_or(block_end);
        (s, end)
    });
    (child_indent, range)
}

fn edit_hermes_yaml(text: &str, name: &str, desired: Option<&Desired>) -> Result<(String, Change)> {
    let parsed: serde_yaml::Value = if text.trim().is_empty() {
        serde_yaml::Value::Mapping(Default::default())
    } else {
        serde_yaml::from_str(text).context("invalid YAML")?
    };
    let existing = parsed.get("mcp_servers").and_then(|s| s.get(name)).cloned();
    let desired_value = desired.map(|d| yaml_desired(existing.as_ref(), d));
    match (&existing, &desired_value) {
        (Some(e), Some(d)) if e == d => return Ok((text.to_string(), Change::Unchanged)),
        (None, None) => return Ok((text.to_string(), Change::NotPresent)),
        _ => {}
    }
    let change = match (&existing, &desired_value) {
        (None, Some(_)) => Change::Added,
        (Some(_), Some(_)) => Change::Updated,
        _ => Change::Removed,
    };

    let lines: Vec<&str> = text.lines().collect();
    let block = lines.iter().position(|l| {
        let t = l.trim_end();
        t == "mcp_servers:"
            || (t.starts_with("mcp_servers:")
                && t["mcp_servers:".len()..].trim_start().starts_with('#'))
    });
    let edited = match block {
        Some(block) => {
            let (indent, range) = yaml_entry_range(&lines, block, name);
            let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
            if let Some((s, e)) = range {
                out.drain(s..e);
            }
            if let Some(entry) = &desired_value {
                for (i, line) in yaml_entry_lines(name, entry, indent)?
                    .into_iter()
                    .enumerate()
                {
                    out.insert(block + 1 + i, line);
                }
            }
            let mut s = out.join("\n");
            s.push('\n');
            Some(s)
        }
        None if parsed.get("mcp_servers").is_none() => match &desired_value {
            Some(entry) => {
                let mut s = text.trim_end().to_string();
                if !s.is_empty() {
                    s.push_str("\n\n");
                }
                s.push_str("mcp_servers:\n");
                for line in yaml_entry_lines(name, entry, 2)? {
                    s.push_str(&line);
                    s.push('\n');
                }
                Some(s)
            }
            None => None,
        },
        // Inline/flow-style `mcp_servers:` — fall back to a structured rewrite.
        None => None,
    };
    // Verify the text edit; fall back to a (comment-losing) structured rewrite.
    if let Some(edited) = edited {
        if let Ok(check) = serde_yaml::from_str::<serde_yaml::Value>(&edited) {
            if check.get("mcp_servers").and_then(|s| s.get(name)).cloned() == desired_value {
                return Ok((edited, change));
            }
        }
    }
    let mut value = parsed;
    let map = value
        .as_mapping_mut()
        .ok_or_else(|| anyhow!("top level is not a YAML mapping"))?;
    let servers = map
        .entry("mcp_servers".into())
        .or_insert_with(|| serde_yaml::Value::Mapping(Default::default()));
    let servers = servers
        .as_mapping_mut()
        .ok_or_else(|| anyhow!("mcp_servers is not a mapping"))?;
    match desired_value {
        Some(d) => {
            servers.insert(name.into(), d);
        }
        None => {
            servers.remove(name);
        }
    }
    Ok((serde_yaml::to_string(&value)?, change))
}

fn hermes_configured(text: &str, name: &str) -> bool {
    serde_yaml::from_str::<serde_yaml::Value>(text)
        .ok()
        .and_then(|v| v.get("mcp_servers").and_then(|s| s.get(name)).map(|_| ()))
        .is_some()
}

// ── File writing ───────────────────────────────────────────────────────────────

/// Replace `path` atomically, following symlinks (dotfile managers) and
/// preserving permissions; keeps the previous content as `<file>.trace-backup`.
fn write_config(path: &Path, content: &str) -> Result<()> {
    let target = if path.exists() {
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
    } else {
        path.to_path_buf()
    };
    let dir = target
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", target.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let file_name = target
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    if target.exists() {
        std::fs::copy(&target, dir.join(format!("{file_name}.trace-backup")))
            .with_context(|| format!("backing up {}", target.display()))?;
    }
    let tmp = dir.join(format!(".{file_name}.trace-tmp-{}", std::process::id()));
    std::fs::write(&tmp, content).with_context(|| format!("writing {}", tmp.display()))?;
    if let Ok(meta) = std::fs::metadata(&target) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, &target).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow!("replacing {}: {e}", target.display())
    })
}

/// Compute the new content for one agent's config. `desired = None` removes.
fn plan_edit(agent: &DiscoveredAgent, desired: Option<&Desired>) -> Result<(String, Change)> {
    let text = if agent.exists {
        std::fs::read_to_string(&agent.config_path)
            .with_context(|| format!("reading {}", agent.config_path.display()))?
    } else {
        String::new()
    };
    match agent.spec.format {
        Format::Json { key, style } => {
            let mut value = parse_json_config(&text)?;
            let change = match desired {
                Some(d) => {
                    let existing = value.get(key).and_then(|s| s.get(SERVER_NAME));
                    let entry = merge_json_entry(existing, style, d);
                    upsert_json(&mut value, key, SERVER_NAME, entry)?
                }
                None => remove_json(&mut value, key, SERVER_NAME),
            };
            Ok((render_json(&value)?, change))
        }
        Format::CodexToml => edit_codex_toml(&text, SERVER_NAME, desired),
        Format::HermesYaml => edit_hermes_yaml(&text, SERVER_NAME, desired),
    }
}

fn is_configured(agent: &DiscoveredAgent) -> Result<bool> {
    if !agent.exists {
        return Ok(false);
    }
    let text = std::fs::read_to_string(&agent.config_path)?;
    Ok(match agent.spec.format {
        Format::Json { key, .. } => json_configured(&parse_json_config(&text)?, key, SERVER_NAME),
        Format::CodexToml => codex_configured(&text, SERVER_NAME),
        Format::HermesYaml => hermes_configured(&text, SERVER_NAME),
    })
}

/// Resolve the path to the trace binary: the running executable, else
/// `trace` on PATH.
fn resolve_trace_binary() -> Result<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if exe.is_file() {
            return Ok(exe.canonicalize().unwrap_or(exe));
        }
    }
    let exe_name = format!("trace{}", std::env::consts::EXE_SUFFIX);
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join(&exe_name))
        .find(|p| p.is_file())
        .ok_or_else(|| anyhow!("trace binary not found; install it first"))
}

fn display_path(path: &Path) -> String {
    match home() {
        Some(h) => match path.strip_prefix(&h) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        },
        None => path.display().to_string(),
    }
}

/// Write `~/.trace/discovery.json` so other tools can find trace.
fn write_discovery_registry(
    binary: &Path,
    args: &[String],
    registered: &[&str],
) -> Result<PathBuf> {
    let dir = crate::daemon::trace_home_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let registry = json!({
        "binary": binary.display().to_string(),
        "version": env!("CARGO_PKG_VERSION"),
        "mcp": { "transport": "stdio", "command": binary.display().to_string(), "args": args },
        "socket_base_dir": dir.display().to_string(),
        "socket_uri": "mcp://trace",
        "registered_agents": registered,
        "timestamp": crate::humanize::now_ms(),
    });
    let path = dir.join("discovery.json");
    std::fs::write(&path, render_json(&registry)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Options for [`run`].
#[derive(Debug, Default, Clone)]
pub struct SetupOptions {
    pub dry_run: bool,
    pub remove: bool,
    /// Pin this project root in the registered command.
    pub root: Option<PathBuf>,
}

/// Register (or remove) trace with every discovered agent.
pub fn run(options: &SetupOptions) -> Result<()> {
    let binary = resolve_trace_binary()?;
    let command = binary.display().to_string();
    let mut args = vec!["serve".to_string()];
    if let Some(root) = &options.root {
        args.push(root.display().to_string());
    }
    let agents = discover_agents();
    if agents.is_empty() {
        println!("No supported AI agents found on this system.");
        println!(
            "Supported: {}",
            AGENTS.iter().map(|a| a.name).collect::<Vec<_>>().join(", ")
        );
        return Ok(());
    }

    let mut registered = Vec::new();
    let mut failed = 0;
    let mut global_only = Vec::new();
    for agent in &agents {
        let desired = Desired {
            command: command.as_str(),
            args: args.as_slice(),
            args_explicit: options.root.is_some(),
        };
        let desired = (!options.remove).then_some(&desired);
        let label = format!("{} ({})", agent.spec.name, display_path(&agent.config_path));
        match plan_edit(agent, desired) {
            Ok((content, change)) => {
                if change.writes() && !options.dry_run {
                    if let Err(e) = write_config(&agent.config_path, &content) {
                        println!("✗ {label}: {e:#}");
                        failed += 1;
                        continue;
                    }
                }
                let mark = if change.writes() { "✓" } else { "=" };
                println!("{mark} {label}: {}", change.describe(options.dry_run));
                if !options.remove && change != Change::NotPresent {
                    registered.push(agent.spec.name);
                    if !agent.spec.project_aware && options.root.is_none() {
                        global_only.push(agent.spec.name);
                    }
                }
            }
            Err(e) => {
                failed += 1;
                println!("✗ {label}: {e:#}");
                if !options.remove {
                    if let Format::Json { key, style } = agent.spec.format {
                        let snippet =
                            json!({ key: { SERVER_NAME: json_entry(style, &command, &args) } });
                        println!("    add manually: {}", serde_json::to_string(&snippet)?);
                    }
                }
            }
        }
    }

    println!();
    if options.dry_run {
        println!("Dry run: nothing was written.");
        return Ok(());
    }
    if options.remove {
        let registry = crate::daemon::trace_home_dir().join("discovery.json");
        let _ = std::fs::remove_file(registry);
        println!("Removal complete ({failed} failed).");
    } else {
        println!(
            "Registration complete: {} configured, {failed} failed.",
            registered.len()
        );
        let path = write_discovery_registry(&binary, &args, &registered)?;
        println!("Discovery registry written to {}", display_path(&path));
        if !global_only.is_empty() {
            println!();
            println!(
                "Note: {} start MCP servers without a project directory. Re-run with `trace setup --root /path/to/project`, \
                 or edit the entry's args to [\"serve\", \"/path/to/project\"].",
                global_only.join(", ")
            );
        }
        println!("Restart your agents to pick up the new server.");
    }
    if failed > 0 {
        return Err(anyhow!("{failed} agent config(s) could not be updated"));
    }
    Ok(())
}

/// Compatibility entry point: register with every discovered agent.
pub fn auto_detect_and_register() -> Result<()> {
    run(&SetupOptions::default())
}

/// List all discovered AI agents without modifying their configs.
pub fn list_agents() -> Result<()> {
    let agents = discover_agents();
    if agents.is_empty() {
        println!("No supported AI agents found.");
        println!(
            "Supported: {}",
            AGENTS.iter().map(|a| a.name).collect::<Vec<_>>().join(", ")
        );
        return Ok(());
    }
    println!("Discovered AI agents ({}):", agents.len());
    for agent in &agents {
        let state = match is_configured(agent) {
            Ok(true) => "trace configured".to_string(),
            Ok(false) if agent.exists => "trace not configured".to_string(),
            Ok(false) => "config file will be created by `trace setup`".to_string(),
            Err(e) => format!("config unreadable: {e:#}"),
        };
        let format = match agent.spec.format {
            Format::Json { .. } => "JSON",
            Format::CodexToml => "TOML",
            Format::HermesYaml => "YAML",
        };
        println!(
            "  {} — {} [{format}]",
            agent.spec.name,
            display_path(&agent.config_path)
        );
        println!("    {state}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    static SERVE: std::sync::LazyLock<Vec<String>> =
        std::sync::LazyLock::new(|| vec!["serve".to_string()]);

    fn args() -> Vec<String> {
        SERVE.clone()
    }

    fn want(command: &'static str) -> Desired<'static> {
        Desired {
            command,
            args: &SERVE,
            args_explicit: false,
        }
    }

    #[test]
    fn json_upsert_creates_key_and_preserves_order() {
        let mut v = parse_json_config(r#"{"zeta":1,"alpha":{"x":true}}"#).unwrap();
        let change = upsert_json(
            &mut v,
            "mcpServers",
            "trace",
            json_entry(EntryStyle::Standard, "/bin/trace", &args()),
        )
        .unwrap();
        assert_eq!(change, Change::Added);
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec!["zeta", "alpha", "mcpServers"],
            "key order is preserved"
        );
        assert_eq!(v["mcpServers"]["trace"]["command"], "/bin/trace");
        assert_eq!(v["mcpServers"]["trace"]["args"][0], "serve");
        let again = upsert_json(
            &mut v,
            "mcpServers",
            "trace",
            json_entry(EntryStyle::Standard, "/bin/trace", &args()),
        )
        .unwrap();
        assert_eq!(again, Change::Unchanged);
        let moved = upsert_json(
            &mut v,
            "mcpServers",
            "trace",
            json_entry(EntryStyle::Standard, "/usr/bin/trace", &args()),
        )
        .unwrap();
        assert_eq!(moved, Change::Updated);
    }

    #[test]
    fn json_preserves_existing_servers_and_removes_cleanly() {
        let mut v = parse_json_config(r#"{"mcpServers":{"existing":{"command":"foo","args":[]}}}"#)
            .unwrap();
        upsert_json(
            &mut v,
            "mcpServers",
            "trace",
            json_entry(EntryStyle::ClaudeCode, "/bin/trace", &args()),
        )
        .unwrap();
        assert_eq!(v["mcpServers"]["existing"]["command"], "foo");
        assert_eq!(v["mcpServers"]["trace"]["type"], "stdio");
        assert_eq!(remove_json(&mut v, "mcpServers", "trace"), Change::Removed);
        assert_eq!(
            remove_json(&mut v, "mcpServers", "trace"),
            Change::NotPresent
        );
        assert_eq!(v["mcpServers"]["existing"]["command"], "foo");
    }

    #[test]
    fn json_rejects_comments_and_non_objects() {
        assert!(parse_json_config("// comment\n{}").is_err());
        assert!(parse_json_config("[]").is_err());
        assert!(parse_json_config("  ").unwrap().is_object());
        let mut v = json!({"mcpServers": 3});
        assert!(upsert_json(&mut v, "mcpServers", "trace", json!({})).is_err());
    }

    #[test]
    fn opencode_and_vscode_entry_shapes() {
        let oc = json_entry(EntryStyle::OpenCode, "/bin/trace", &args());
        assert_eq!(
            oc,
            json!({"type": "local", "command": ["/bin/trace", "serve"], "enabled": true})
        );
        let vs = json_entry(EntryStyle::VsCode, "/bin/trace", &args());
        assert_eq!(vs["type"], "stdio");
    }

    #[test]
    fn codex_toml_edit_preserves_comments() {
        let text = "# my codex config\nmodel = \"gpt-5\" # inline comment\n\n[mcp_servers.other]\ncommand = \"x\"\n";
        let (out, change) = edit_codex_toml(text, "trace", Some(&want("/bin/trace"))).unwrap();
        assert_eq!(change, Change::Added);
        assert!(out.contains("# my codex config"));
        assert!(out.contains("# inline comment"));
        assert!(out.contains("[mcp_servers.trace]"), "{out}");
        assert!(out.contains("[mcp_servers.other]"));
        let parsed: toml::Table = toml::from_str(&out).unwrap();
        assert_eq!(
            parsed["mcp_servers"]["trace"]["command"].as_str(),
            Some("/bin/trace")
        );

        let (same, change) = edit_codex_toml(&out, "trace", Some(&want("/bin/trace"))).unwrap();
        assert_eq!(change, Change::Unchanged);
        assert_eq!(same, out);
        let (removed, change) = edit_codex_toml(&out, "trace", None).unwrap();
        assert_eq!(change, Change::Removed);
        assert!(!removed.contains("mcp_servers.trace"));
        assert!(codex_configured(&out, "trace"));
    }

    #[test]
    fn codex_toml_on_empty_file() {
        let (out, change) = edit_codex_toml("", "trace", Some(&want("/bin/trace"))).unwrap();
        assert_eq!(change, Change::Added);
        assert!(
            !out.contains("[mcp_servers]\n"),
            "parent table stays implicit: {out}"
        );
        assert!(out.contains("[mcp_servers.trace]"));
    }

    #[test]
    fn hermes_yaml_appends_block_and_keeps_comments() {
        let text = "# Hermes config\nmodel: hermes-4 # the model\n";
        let (out, change) = edit_hermes_yaml(text, "trace", Some(&want("/bin/trace"))).unwrap();
        assert_eq!(change, Change::Added);
        assert!(out.contains("# Hermes config"));
        assert!(out.contains("# the model"));
        let v: serde_yaml::Value = serde_yaml::from_str(&out).unwrap();
        assert_eq!(
            v["mcp_servers"]["trace"]["command"].as_str(),
            Some("/bin/trace")
        );
        assert_eq!(v["model"].as_str(), Some("hermes-4"));
        let (_, again) = edit_hermes_yaml(&out, "trace", Some(&want("/bin/trace"))).unwrap();
        assert_eq!(again, Change::Unchanged);
    }

    #[test]
    fn hermes_yaml_inserts_into_existing_block_and_replaces_entry() {
        let text = "mcp_servers:  # servers\n    # a comment inside\n    github:\n        command: npx\n        args: [\"gh\"]\n    trace:\n        command: /old/trace\n        args: [serve]\nother: 1\n";
        let (out, change) = edit_hermes_yaml(text, "trace", Some(&want("/new/trace"))).unwrap();
        assert_eq!(change, Change::Updated);
        assert!(out.contains("# a comment inside"));
        assert!(!out.contains("/old/trace"));
        let v: serde_yaml::Value = serde_yaml::from_str(&out).unwrap();
        assert_eq!(
            v["mcp_servers"]["trace"]["command"].as_str(),
            Some("/new/trace")
        );
        assert_eq!(v["mcp_servers"]["github"]["command"].as_str(), Some("npx"));
        assert_eq!(v["other"].as_i64(), Some(1));

        let (removed, change) = edit_hermes_yaml(&out, "trace", None).unwrap();
        assert_eq!(change, Change::Removed);
        let v: serde_yaml::Value = serde_yaml::from_str(&removed).unwrap();
        assert!(v["mcp_servers"].get("trace").is_none());
        assert!(v["mcp_servers"].get("github").is_some());
    }

    #[test]
    fn hermes_yaml_flow_style_falls_back_to_structured_rewrite() {
        let (out, change) =
            edit_hermes_yaml("mcp_servers: {}\n", "trace", Some(&want("/bin/trace"))).unwrap();
        assert_eq!(change, Change::Added);
        assert!(hermes_configured(&out, "trace"));
    }

    #[test]
    fn existing_json_customisations_survive_reinstall() {
        let pinned = json!({"command": "/old/trace", "args": ["serve", "/home/me/proj"], "env": {"TRACE_REFRESH_MS": "5000"}, "type": "stdio"});
        let merged = merge_json_entry(Some(&pinned), EntryStyle::ClaudeCode, &want("/new/trace"));
        assert_eq!(merged["command"], "/new/trace");
        assert_eq!(
            merged["args"],
            json!(["serve", "/home/me/proj"]),
            "pinned root kept"
        );
        assert_eq!(merged["env"]["TRACE_REFRESH_MS"], "5000", "env kept");

        let root_args = vec!["serve".to_string(), "/new/root".to_string()];
        let explicit = Desired {
            command: "/new/trace",
            args: &root_args,
            args_explicit: true,
        };
        let merged = merge_json_entry(Some(&pinned), EntryStyle::Standard, &explicit);
        assert_eq!(
            merged["args"],
            json!(["serve", "/new/root"]),
            "--root replaces args"
        );
        assert_eq!(merged["env"]["TRACE_REFRESH_MS"], "5000");

        let disabled =
            json!({"type": "local", "command": ["/old/trace", "serve", "/p"], "enabled": false});
        let merged = merge_json_entry(Some(&disabled), EntryStyle::OpenCode, &want("/new/trace"));
        assert_eq!(merged["command"], json!(["/new/trace", "serve", "/p"]));
        assert_eq!(merged["enabled"], false, "a disabled server stays disabled");

        let mut config = json!({"mcpServers": {"trace": pinned.clone()}});
        let entry = merge_json_entry(
            config["mcpServers"].get("trace"),
            EntryStyle::ClaudeCode,
            &want("/old/trace"),
        );
        assert_eq!(
            upsert_json(&mut config, "mcpServers", "trace", entry).unwrap(),
            Change::Unchanged
        );
    }

    #[test]
    fn existing_toml_and_yaml_customisations_survive_reinstall() {
        let toml_text = "[mcp_servers.trace]\ncommand = \"/old/trace\"\nargs = [\"serve\", \"/p\"]\nstartup_timeout_sec = 30 # tuned\n";
        let (out, change) = edit_codex_toml(toml_text, "trace", Some(&want("/new/trace"))).unwrap();
        assert_eq!(change, Change::Updated);
        let parsed: toml::Table = toml::from_str(&out).unwrap();
        let entry = &parsed["mcp_servers"]["trace"];
        assert_eq!(entry["command"].as_str(), Some("/new/trace"));
        assert_eq!(
            entry["args"].as_array().unwrap().len(),
            2,
            "pinned root kept"
        );
        assert_eq!(entry["startup_timeout_sec"].as_integer(), Some(30));
        assert!(out.contains("# tuned"));
        let (_, again) = edit_codex_toml(&out, "trace", Some(&want("/new/trace"))).unwrap();
        assert_eq!(again, Change::Unchanged);

        let yaml_text = "mcp_servers:\n  trace:\n    command: /old/trace\n    args: [serve, /p]\n    timeout: 120\n";
        let (out, change) =
            edit_hermes_yaml(yaml_text, "trace", Some(&want("/new/trace"))).unwrap();
        assert_eq!(change, Change::Updated);
        let v: serde_yaml::Value = serde_yaml::from_str(&out).unwrap();
        assert_eq!(
            v["mcp_servers"]["trace"]["command"].as_str(),
            Some("/new/trace")
        );
        assert_eq!(v["mcp_servers"]["trace"]["timeout"].as_i64(), Some(120));
        assert_eq!(v["mcp_servers"]["trace"]["args"][1].as_str(), Some("/p"));
    }

    #[test]
    fn write_config_is_atomic_backs_up_and_follows_symlinks() {
        let dir = TempDir::new().unwrap();
        let real = dir.path().join("real.json");
        std::fs::write(&real, "{}").unwrap();
        #[cfg(unix)]
        {
            let link = dir.path().join("link.json");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            write_config(&link, "{\"a\":1}\n").unwrap();
            assert!(
                std::fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "symlink kept"
            );
        }
        #[cfg(not(unix))]
        write_config(&real, "{\"a\":1}\n").unwrap();
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "{\"a\":1}\n");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("real.json.trace-backup")).unwrap(),
            "{}"
        );
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("trace-tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn plan_edit_creates_missing_config() {
        let dir = TempDir::new().unwrap();
        let agent = DiscoveredAgent {
            spec: &AGENTS[2], // Cursor
            config_path: dir.path().join("mcp.json"),
            exists: false,
        };
        let (content, change) = plan_edit(&agent, Some(&want("/bin/trace"))).unwrap();
        assert_eq!(change, Change::Added);
        let v: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["mcpServers"]["trace"]["command"], "/bin/trace");
    }

    #[test]
    fn resolve_trace_binary_finds_an_existing_file() {
        let path = resolve_trace_binary().unwrap();
        assert!(path.is_file());
    }
}
