//! dialogos: a passive AI assistant on the Logos chat network.
//!
//! Startup: load config, open the Logos client, print the address to share out
//! of band, then run the event loop until the process is stopped.
#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;

use dialogos::bot;
use dialogos::config::Config;
use dialogos::llm::{LlmBackend, OpenAiCompatBackend};

fn main() -> anyhow::Result<()> {
    init_tracing();

    let config_path = config_path_from_args()?;
    let cfg = Config::load(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    let backend: Arc<dyn LlmBackend> = Arc::new(OpenAiCompatBackend::new(&cfg.llm)?);

    let mut logos_cfg =
        logos_chat::LogosConfig::new(cfg.identity.db_path.clone(), cfg.identity.db_key.clone());
    if let Some(url) = &cfg.identity.registry_url {
        logos_cfg.set_registry_url(url.clone());
    }

    let (client, events) = logos_chat::open(logos_cfg)
        .map_err(|e| anyhow::anyhow!("opening the Logos client: {e}"))?;

    // The address is freshly generated this run and not persisted: it changes
    // on every restart, and prior conversations are permanently lost. See the
    // README's "Accepted limitations".
    let address = client.addr().to_string();
    tracing::info!(address = %address, "dialogos online");
    println!("dialogos address (share this out of band): {address}");

    // SIGINT/SIGTERM (systemd stop) trigger a graceful drain, not an abrupt kill.
    let (shutdown_tx, shutdown_rx) = crossbeam_channel::bounded::<()>(1);
    ctrlc::set_handler(move || {
        let _ = shutdown_tx.try_send(());
    })
    .context("installing the shutdown signal handler")?;

    bot::run(events, client, backend, &cfg, shutdown_rx);
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}

fn config_path_from_args() -> anyhow::Result<PathBuf> {
    let mut args = std::env::args().skip(1);
    let mut path = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" | "-c" => {
                let value = args.next().context("--config requires a path argument")?;
                path = Some(PathBuf::from(value));
            }
            "--help" | "-h" => {
                println!("usage: dialogos [--config <path>]   (default: dialogos.toml)");
                std::process::exit(0);
            }
            other => anyhow::bail!("unexpected argument: {other}"),
        }
    }
    Ok(path.unwrap_or_else(|| PathBuf::from("dialogos.toml")))
}
