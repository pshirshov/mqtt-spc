mod bridge;
mod config;
mod edp;
mod mqtt;

use clap::Parser;
use tokio::sync::mpsc;
use tracing::error;
use tracing_subscriber::EnvFilter;

use edp::session::LINK_EVENT_QUEUE_LEN;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = config::Config::from_args(config::Args::parse());

    let (link_tx, link_rx) = mpsc::channel(LINK_EVENT_QUEUE_LEN);
    let receiver_config = config.receiver.clone();
    let receiver = tokio::spawn(async move {
        if let Err(e) = edp::session::run_receiver(receiver_config, link_tx).await {
            error!("EDP receiver failed: {e}");
        }
    });

    bridge::run(&config, link_rx).await;
    receiver.abort();
    std::process::exit(1);
}
