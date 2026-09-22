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
    /// Run the MCP server over stdio (Phase 1-4 tools)
    Serve {
        /// Project root (default: current directory)
        #[arg(default_value = ".")]
        root: String,
    },
    /// Scan a directory and rebuild the structural index (Phase 1)
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
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Commands::Serve { root } => {
            let root = PathBuf::from(&root);
            eprintln!("trace MCP server starting — root: {:?}", root);
            trace::mcp::run_mcp_server(root)?;
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
    }
}
