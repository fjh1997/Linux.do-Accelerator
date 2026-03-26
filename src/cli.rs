use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::config::AppConfig;
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
use crate::gui;
use crate::service;

#[derive(Debug, Parser)]
#[command(name = "linuxdo-accelerator")]
#[command(about = "linux.do accelerator CLI")]
pub struct Cli {
    #[arg(long)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Gui,
    InitConfig,
    Setup,
    Run,
    Start,
    Stop,
    Status,
    CleanHosts,
    ApplyHosts,
    BackupHosts,
    RestoreHosts,
    UninstallCert,
    Cleanup,
    #[command(hide = true)]
    ConfigJson,
    #[command(hide = true)]
    HelperStart,
    #[command(hide = true)]
    HelperStop,
    #[command(hide = true)]
    Daemon,
    #[command(hide = true)]
    TrayShell,
}

pub fn run(cli: Cli) -> Result<()> {
    match cli.command {
        None | Some(Command::Gui) => {
            #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
            {
                let config_path = service::init_config(cli.config.clone())?;
                gui::run(config_path)?;
            }
            #[cfg(target_os = "android")]
            {
                anyhow::bail!("GUI is not supported on Android yet; use CLI subcommands");
            }
        }
        Some(Command::InitConfig) => {
            let config_path = service::init_config(cli.config)?;
            println!("config ready: {}", config_path.display());
        }
        Some(Command::Setup) => {
            service::setup(cli.config)?;
            println!("setup complete");
        }
        Some(Command::Run) => {
            run_async(service::run_foreground(cli.config, false))?;
        }
        Some(Command::Start) => {
            run_async(service::run_foreground(cli.config, true))?;
        }
        Some(Command::Stop) | Some(Command::HelperStop) => {
            service::helper_stop(cli.config)?;
            println!("service stopped");
        }
        Some(Command::Status) => {
            let status = service::status(cli.config)?;
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        Some(Command::CleanHosts) => {
            service::clean_hosts(cli.config)?;
            println!("hosts cleaned");
        }
        Some(Command::ApplyHosts) => {
            service::apply_hosts_only(cli.config)?;
            println!("hosts applied");
        }
        Some(Command::BackupHosts) => {
            service::backup_hosts(cli.config)?;
            println!("hosts backup ready");
        }
        Some(Command::RestoreHosts) => {
            service::restore_hosts(cli.config)?;
            println!("hosts restored");
        }
        Some(Command::UninstallCert) => {
            service::uninstall_certificate(cli.config)?;
            println!("certificate removed");
        }
        Some(Command::Cleanup) => {
            service::cleanup(cli.config)?;
            println!("cleanup complete");
        }
        Some(Command::ConfigJson) => {
            let paths = service::resolve_paths(cli.config)?;
            let config = AppConfig::load_or_create(&paths.config_path)?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        Some(Command::HelperStart) => {
            service::helper_start(cli.config)?;
            println!("service started");
        }
        Some(Command::Daemon) => {
            run_async(service::run_foreground(cli.config, false))?;
        }
        Some(Command::TrayShell) => {
            #[cfg(target_os = "linux")]
            {
                let config_path = service::init_config(cli.config)?;
                gui::run_tray_shell(config_path)?;
            }
            #[cfg(not(target_os = "linux"))]
            {
                anyhow::bail!("tray-shell is only supported on Linux");
            }
        }
    }

    Ok(())
}

fn run_async<F>(future: F) -> Result<()>
where
    F: std::future::Future<Output = Result<()>>,
{
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(future)
}
