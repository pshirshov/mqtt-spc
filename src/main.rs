mod bridge;
mod config;
mod model;
mod mqtt;
mod spc;

use std::path::Path;

use clap::Parser;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = config::Args::parse();
    let spc_creds_path = args.spc_creds.clone();
    let config = config::Config::from_args(args);

    let creds = config::Credentials::load(Path::new(&spc_creds_path));
    let spc = spc::client::SpcClient::new(&config.spc_url, &creds);

    bridge::run(&config, spc).await;
}
