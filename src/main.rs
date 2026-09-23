use anyhow::{bail, Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use serde_json::json;
use std::path::PathBuf;
use std::process::ExitCode;
use trace::invariant::{self, PlannedFile, Severity};
use trace::mcp::{Refresh, Server};

const LONG_ABOUT: &str = "\
Architectural memory engine — an MCP server for codebase structure, guardrails, decisions and session history.

trace gives AI coding agents structured, always-fresh knowledge of a repository (symbols, call graph, \
imports, HTTP routes), the project's architectural rules, its decision records (ADRs) and a log of \
earlier agent sessions. Register it once with `trace setup`; agents then launch `trace serve`, which \
connects them to one shared per-project daemon.";

const EXAMPLES: &str = "\
Examples:
  trace setup                          Register the MCP server with installed AI agents
  trace status                         Show index, rules, decisions and daemon state
  trace scan .                         Index the current project and print statistics
  trace check --strict                 Enforce architectural rules (non-zero exit on violations)
  git diff --name-only | xargs trace check
  trace service install ~/code/app     Keep a project's daemon running at login
  trace completions zsh > ~/.zfunc/_trace

Guide: https://github.com/ayoubzulfiqar/trace/blob/main/docs/USAGE.md";

#[derive(Parser)]
#[command(
    name = "trace",
    version,
    about = "Architectural memory engine — an MCP server for codebase structure, guardrails, decisions and session history",
    long_about = LONG_ABOUT,
    after_help = "Guide: https://github.com/ayoubzulfiqar/trace/blob/main/docs/USAGE.md",
    after_long_help = EXAMPLES
)]
struct Cli {
    #[command(subcommand)]
    cmd: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Serve MCP over stdio (proxies to the shared per-project daemon)
    Serve {
        /// Project root (default: $TRACE_ROOT, else discovered from the current directory)
        root: Option<PathBuf>,
        /// Serve in this process instead of through the daemon
        #[arg(long)]
        inline: bool,
    },
    /// Run the per-project background daemon (Unix socket)
    #[cfg_attr(not(unix), command(hide = true))]
    Daemon {
        /// Project root (default: discovered from the current directory)
        root: Option<PathBuf>,
        /// Exit after this many seconds without connections (0 = never)
        #[arg(long, default_value_t = 0)]
        idle_timeout: u64,
    },
    /// Index a repository (incrementally) and print statistics
    Scan {
        /// Project root (default: discovered from the current directory)
        root: Option<PathBuf>,
        /// Re-parse every file instead of only changed ones
        #[arg(long)]
        full: bool,
        /// Discard the cached index and rebuild it from scratch
        #[arg(long, conflicts_with = "full")]
        reset: bool,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// Check files against the architectural rules; exits 1 on blocking violations (CI / git hooks)
    Check {
        /// Files to check (default: every source file in the project)
        files: Vec<String>,
        /// Project root (default: discovered from the current directory)
        #[arg(long)]
        root: Option<PathBuf>,
        /// Also fail on warnings
        #[arg(long)]
        strict: bool,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// Show project, index, rules, decisions, history and daemon status
    Status {
        /// Project root (default: discovered from the current directory)
        root: Option<PathBuf>,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// Register trace with installed AI agents
    Setup {
        /// Show what would change without writing anything
        #[arg(long)]
        dry_run: bool,
        /// Remove the trace entry from every agent instead
        #[arg(long)]
        remove: bool,
        /// Pin a project root in the registered command (default: agents' working directory)
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// List discovered AI agents and whether trace is configured (read-only)
    ListAgents,
    /// Manage the per-project system service (systemd user unit / launchd agent)
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Print a shell completion script (bash, zsh, fish, elvish, powershell)
    Completions {
        /// Target shell
        shell: clap_complete::Shell,
    },
    /// Print the man page (roff) to stdout
    Man,
    #[command(hide = true)]
    Test,
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Install and start a service running the daemon for a project
    Install {
        /// Project root (default: discovered from the current directory)
        root: Option<PathBuf>,
    },
    /// Stop and remove the project's service
    Uninstall {
        /// Project root (default: discovered from the current directory)
        root: Option<PathBuf>,
    },
    /// Show whether the project's service is installed and running
    Status {
        /// Project root (default: discovered from the current directory)
        root: Option<PathBuf>,
    },
}

/// Resolve the project root: explicit argument, then `$TRACE_ROOT`, then
/// upward discovery from the current directory, then the current directory.
fn resolve_root(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("cannot determine the current directory")?;
    let explicit = explicit.or_else(|| {
        std::env::var_os("TRACE_ROOT")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    });
    match explicit {
        Some(path) => {
            let abs = if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            };
            if !abs.is_dir() {
                bail!(
                    "project root {} does not exist or is not a directory",
                    abs.display()
                );
            }
            Ok(abs.canonicalize()?)
        }
        None => Ok(trace::root::resolve(None, &cwd).unwrap_or(cwd)),
    }
}

/// CLI subcommands should die quietly on a closed pipe (`trace ... | head`)
/// like any Unix tool; the servers keep Rust's default (SIGPIPE ignored, so
/// a vanished client surfaces as a write error instead of killing them).
#[cfg(unix)]
fn restore_default_sigpipe() {
    // SAFETY: called before any threads are spawned; only resets a disposition.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    #[cfg(unix)]
    if !matches!(cli.cmd, Commands::Serve { .. } | Commands::Daemon { .. }) {
        restore_default_sigpipe();
    }
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    match cli.cmd {
        Commands::Serve { root, inline } => {
            let root = resolve_root(root)?;
            eprintln!(
                "trace {} MCP server — root: {}",
                env!("CARGO_PKG_VERSION"),
                root.display()
            );
            if inline {
                trace::daemon::run_stdio(root)?;
            } else {
                trace::daemon::run_shim(root)?;
            }
        }
        Commands::Daemon { root, idle_timeout } => {
            let root = resolve_root(root)?;
            #[cfg(unix)]
            {
                let timeout =
                    (idle_timeout > 0).then(|| std::time::Duration::from_secs(idle_timeout));
                trace::daemon::run_daemon(root, timeout)?;
            }
            #[cfg(not(unix))]
            {
                let _ = (root, idle_timeout);
                bail!("daemon mode needs Unix sockets (Linux/macOS); `trace serve` runs inline on this platform");
            }
        }
        Commands::Scan {
            root,
            full,
            reset,
            json,
        } => return scan(resolve_root(root)?, full, reset, json),
        Commands::Check {
            files,
            root,
            strict,
            json,
        } => return check(resolve_root(root)?, files, strict, json),
        Commands::Status { root, json } => return status(resolve_root(root)?, json),
        Commands::Setup {
            dry_run,
            remove,
            root,
        } => {
            let root = root.map(|r| resolve_root(Some(r))).transpose()?;
            trace::setup::run(&trace::setup::SetupOptions {
                dry_run,
                remove,
                root,
            })?;
        }
        Commands::ListAgents => trace::setup::list_agents()?,
        Commands::Service { action } => match action {
            ServiceAction::Install { root } => trace::service::install(&resolve_root(root)?)?,
            ServiceAction::Uninstall { root } => trace::service::uninstall(&resolve_root(root)?)?,
            ServiceAction::Status { root } => trace::service::status(&resolve_root(root)?)?,
        },
        Commands::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "trace", &mut std::io::stdout());
        }
        Commands::Man => {
            clap_mangen::Man::new(Cli::command()).render(&mut std::io::stdout())?;
        }
        Commands::Test => eprintln!("Run `cargo test` instead"),
    }
    Ok(ExitCode::SUCCESS)
}

fn scan(root: PathBuf, full: bool, reset: bool, as_json: bool) -> Result<ExitCode> {
    let server = Server::new(root.clone());
    if let Some(reason) = server.blocked_reason() {
        bail!("{reason}");
    }
    if !as_json {
        eprintln!("Scanning {} ...", root.display());
    }
    let mode = match (reset, full) {
        (true, _) => Refresh::Reset,
        (_, true) => Refresh::Full,
        _ => Refresh::Now,
    };
    let stats = server
        .ensure_index(mode)
        .map_err(anyhow::Error::msg)?
        .unwrap_or_default();
    let index = server.index_stats();
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "scan": stats, "index": index }))?
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "Scan complete: {} files ({} parsed, {} unchanged, {} touched, {} removed), {} errors, {}ms",
        stats.files_total, stats.reparsed, stats.skipped, stats.touched, stats.removed, stats.errors, stats.elapsed_ms
    );
    if stats.oversized > 0 {
        println!("  Skipped {} file(s) over the size limit", stats.oversized);
    }
    println!("  Symbols:    {}", index.symbols);
    println!("  Imports:    {}", index.imports);
    println!("  Call edges: {}", index.call_edges);
    println!("  Routes:     {}", index.routes);
    if !index.languages.is_empty() {
        let langs: Vec<String> = index
            .languages
            .iter()
            .map(|(l, n)| format!("{l} {n}"))
            .collect();
        println!("  Languages:  {}", langs.join(", "));
    }
    if index.files_with_parse_errors > 0 {
        println!(
            "  {} file(s) had syntax errors (facts may be partial)",
            index.files_with_parse_errors
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn check(root: PathBuf, files: Vec<String>, strict: bool, as_json: bool) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let planned: Vec<PlannedFile> = if files.is_empty() {
        trace::scan::collect_files(&root)
            .files
            .into_iter()
            .map(|f| PlannedFile::path(f.rel))
            .collect()
    } else {
        files
            .into_iter()
            .map(|f| {
                // Paths are relative to the current directory when they exist there.
                let local = cwd.join(&f);
                PlannedFile::path(if local.exists() {
                    local.to_string_lossy().into_owned()
                } else {
                    f
                })
            })
            .collect()
    };
    let result = invariant::eval_plan(&root, &planned);
    let failed = !result.allowed || (strict && result.warnings > 0);
    if as_json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        for v in &result.violations {
            let location = match v.line {
                Some(line) if !v.file.is_empty() => format!("{}:{line}", v.file),
                _ if !v.file.is_empty() => v.file.clone(),
                _ => "(config)".to_string(),
            };
            let severity = match v.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
                Severity::Info => "info",
            };
            let message = if v.message.is_empty() {
                String::new()
            } else {
                format!(" — {}", v.message)
            };
            println!(
                "{location}: {severity}[{}]: {}{message}",
                v.rule_id, v.detail
            );
        }
        for rejected in &result.rejected_files {
            eprintln!("skipped: {rejected}");
        }
        if result.rules_loaded == 0 && result.config_error.is_none() {
            println!("No architectural rules defined (.architectural-rules.json/.yaml or [[rules]] in trace.toml).");
        } else {
            println!(
                "{}: {} error(s), {} warning(s), {} info across {} file(s) and {} rule(s)",
                if failed { "FAILED" } else { "OK" },
                result.errors,
                result.warnings,
                result.infos,
                result.files_evaluated,
                result.rules_loaded
            );
        }
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn status(root: PathBuf, as_json: bool) -> Result<ExitCode> {
    let server = Server::new(root.clone());
    let blocked = server.blocked_reason().map(str::to_string);
    let (indexed_files, sessions) = server.store_counts();
    let rules = invariant::RulesConfig::load(&root);
    let decisions = trace::adr::list_decisions(&root);
    let daemon = trace::daemon::daemon_status(&root);
    let rules_json = match &rules {
        Ok(loaded) => json!({
            "source": loaded.source.as_ref().map(|p| p.display().to_string()),
            "count": loaded.config.rules.len(),
        }),
        Err(e) => json!({ "error": e }),
    };
    let report = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "root": root.display().to_string(),
        "blocked": blocked,
        "state_dir": root.join(".trace").display().to_string(),
        "indexed_files": indexed_files,
        "sessions": sessions,
        "rules": rules_json,
        "decisions": decisions.len(),
        "decisions_dir": trace::adr::decisions_dir(&root).display().to_string(),
        "daemon": {
            "running": daemon.running,
            "pid": daemon.pid,
            "socket": daemon.socket.display().to_string(),
        },
    });
    if as_json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(ExitCode::SUCCESS);
    }
    println!("trace {}", env!("CARGO_PKG_VERSION"));
    println!("  Project root:  {}", root.display());
    if let Some(reason) = &blocked {
        println!("  ⚠ {reason}");
    }
    println!(
        "  Indexed files: {indexed_files} (cached in {}/trace.db)",
        root.join(".trace").display()
    );
    match &rules {
        Ok(loaded) => match &loaded.source {
            Some(src) => println!(
                "  Rules:         {} from {}",
                loaded.config.rules.len(),
                src.display()
            ),
            None => println!("  Rules:         none defined"),
        },
        Err(e) => println!("  Rules:         INVALID — {e}"),
    }
    println!(
        "  Decisions:     {} in {}",
        decisions.len(),
        trace::adr::decisions_dir(&root).display()
    );
    println!("  Sessions:      {sessions}");
    if daemon.running {
        let pid = daemon
            .pid
            .map(|p| format!(" (pid {p})"))
            .unwrap_or_default();
        println!(
            "  Daemon:        running{pid} on {}",
            daemon.socket.display()
        );
    } else {
        println!("  Daemon:        not running (starts on demand with `trace serve`)");
    }
    Ok(ExitCode::SUCCESS)
}
