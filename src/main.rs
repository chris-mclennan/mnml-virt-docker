mod app;
mod blit;
mod clipboard;
mod config;
mod docker;
mod keys;
mod ui;

use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "mnml-virt-docker",
    version,
    about = "Docker container/image/volume/network/compose browser for mnml"
)]
struct Cli {
    /// Print the resolved config + daemon state and exit.
    #[arg(long)]
    check: bool,
    /// Blit-host mode — render into a UDS-served cell grid instead
    /// of the local terminal.
    #[arg(long, value_name = "SOCKET")]
    blit: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = config::load()?;

    if cli.check {
        println!("config: {}", config::config_path().display());
        for (i, t) in cfg.tabs.iter().enumerate() {
            println!(
                "  tab {} ({}): kind={} project_path={:?}",
                i + 1,
                t.name,
                t.kind,
                t.project_path
            );
        }
        let state = docker::probe_daemon();
        match state {
            docker::DaemonState::Ok(v) => {
                println!("daemon: ok · docker server {v}");
            }
            docker::DaemonState::Offline => {
                println!("daemon: offline (start Docker Desktop, then re-run)");
            }
            docker::DaemonState::CliMissing(e) => {
                println!("daemon: docker CLI not found ({e})");
            }
            docker::DaemonState::Error(e) => {
                println!("daemon: error ({e})");
            }
        }
        println!("(auth: defers to the docker socket — no credentials)");
        return Ok(());
    }

    let mut app = app::App::new(cfg)?;

    if let Some(socket) = cli.blit {
        blit::run(&mut app, std::path::Path::new(&socket)).await
    } else {
        ui::run(&mut app).await
    }
}
