mod config;
mod i18n;
mod manifest;
mod process;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::config::ConfigStore;

#[derive(Debug, Parser)]
#[command(name = "remiaft", version, about = "Minecraft server manager")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Open the interactive terminal UI.
    Tui,
    /// Start a configured server by name or id.
    Start { server: String },
    /// Stop a configured server by name or id.
    Stop { server: String },
    /// Restart a configured server by name or id.
    Restart { server: String },
    /// Print configured servers and runtime state.
    Status,
    /// Fetch recent vanilla Minecraft versions from Mojang metadata.
    Versions {
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
    },
    /// Internal process supervisor. Do not call directly.
    #[command(hide = true)]
    Supervise { server_id: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let store = ConfigStore::new()?;

    match cli.command.unwrap_or(Commands::Tui) {
        Commands::Tui => tui::run(store).await,
        Commands::Start { server } => {
            let config = store.load()?;
            let server = config.find_server(&server)?;
            process::start_supervisor(&store, server)?;
            println!("started {}", server.name);
            Ok(())
        }
        Commands::Stop { server } => {
            let config = store.load()?;
            let server = config.find_server(&server)?;
            process::stop_server(&store, server)?;
            println!("stopped {}", server.name);
            Ok(())
        }
        Commands::Restart { server } => {
            let config = store.load()?;
            let server = config.find_server(&server)?;
            process::stop_server(&store, server)?;
            process::start_supervisor(&store, server)?;
            println!("restarted {}", server.name);
            Ok(())
        }
        Commands::Status => {
            let config = store.load()?;
            for server in &config.servers {
                let status = process::runtime_status(&store, server);
                println!(
                    "{:<20} {:<10} {}",
                    server.name,
                    status.label(),
                    server.directory.display()
                );
            }
            Ok(())
        }
        Commands::Versions { limit } => {
            let versions = manifest::fetch_versions(limit).await?;
            for version in versions {
                println!(
                    "{} {} {}",
                    version.id,
                    version.kind,
                    version.server_url.unwrap_or_default()
                );
            }
            Ok(())
        }
        Commands::Supervise { server_id } => process::run_supervisor(&store, &server_id),
    }
}
