use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "trace",
    about = "Architectural Memory Engine — Rust MCP server"
)]
struct Cli {
    #[clap(subcommand)]
    cmd: Commands,
}

#[derive(clap::Subcommand)]
enum Commands {
    /// Run the MCP server over stdio (connects to daemon if running)
    Serve {
        /// Project root (default: current directory)
        #[arg(default_value = ".")]
        root: String,
    },
    /// Run as a background daemon listening on a Unix socket
    #[cfg_attr(not(unix), allow(dead_code))]
    Daemon {
        /// Project root (default: current directory)
        #[arg(default_value = ".")]
        root: String,
    },
    /// Scan a directory and rebuild the structural index
    Scan {
        /// Project root to scan (default: current directory)
        #[arg(default_value = ".")]
        root: String,
    },
    /// Run all tests
    Test,
    /// Auto-detect installed AI agents and inject MCP server config
    Setup,
    /// List discovered AI agents (read-only)
    ListAgents,
    /// Manage the trace system service
    Service {
        #[clap(subcommand)]
        action: ServiceAction,
    },
}

#[derive(clap::Subcommand)]
enum ServiceAction {
    /// Install trace as a system service (systemd/launchd)
    Install {
        /// Project root to associate with the service
        #[arg(default_value = ".")]
        root: String,
    },
    /// Remove the trace system service
    Uninstall,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Commands::Serve { root } => {
            let root = PathBuf::from(&root);
            eprintln!("trace MCP server starting — root: {:?}", root);
            trace::mcp::run_mcp_shim(root)?;
            Ok(())
        }
        Commands::Daemon { root } => {
            let root = PathBuf::from(&root);
            eprintln!("trace daemon starting — root: {:?}", root);
            trace::mcp::run_mcp_daemon(root)?;
            Ok(())
        }
        Commands::Scan { root } => {
            let root = PathBuf::from(&root);
            eprintln!("Scanning {} ...", root.display());
            let (graph, stats) = trace::scan::scan_repo(&root);
            println!(
                "Scan complete: {} files, {} skipped, {} errors, {}ms",
                stats.reparsed, stats.skipped, stats.errors, stats.elapsed_ms
            );
            println!("  Symbols: {}", graph.symbols.len());
            println!("  Imports: {}", graph.imports.len());
            println!("  Call edges: {}", graph.call_edges.len());
            Ok(())
        }
        Commands::Test => {
            eprintln!("Run `cargo test` instead");
            Ok(())
        }
        Commands::Setup => {
            trace::setup::auto_detect_and_register()?;
            Ok(())
        }
        Commands::ListAgents => {
            trace::setup::list_agents()?;
            Ok(())
        }
        Commands::Service { action } => {
            match action {
                ServiceAction::Install { root } => {
                    let root = PathBuf::from(&root);
                    trace::service::install(&root)?;
                }
                ServiceAction::Uninstall => {
                    trace::service::uninstall()?;
                }
            }
            Ok(())
        }
    }
}
