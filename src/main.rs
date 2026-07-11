mod countdowns;
mod mc;
mod server;

use anyhow::{Context, Result};
use serde::Deserialize;
use std::{fs, str::FromStr};
use tokio::task::JoinSet;
use tracing::warn;

use crate::server::start_server;

#[derive(Deserialize)]
struct Config {
    log_level: String,
    port: u16,
}

fn load_config() -> Result<Config> {
    let config_str = fs::read_to_string("config.yaml").context("Failed to open config file.")?;
    let config =
        serde_yaml::from_str::<Config>(&config_str).context("Failed to parse config file.")?;

    let level = tracing::Level::from_str(&config.log_level).context("Failed to parse log level")?;
    tracing_subscriber::fmt().with_max_level(level).init();
    Ok(config)
}

#[tokio::main]
async fn main() {
    let config = match load_config() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("{err:?}");
            return;
        }
    };
    let mut join_set = JoinSet::new();
    if let Err(err) = start_server(&mut join_set, config).await {
        eprintln!("Failed to start server: {err:?}");
        return;
    }
    match join_set.join_next().await {
        Some(Ok(Ok(()))) => {
            warn!("An actor shutdown");
        }
        Some(Ok(Err(err))) => {
            warn!("An actor stopped with an error: {err:?}");
        }
        Some(Err(err)) => {
            warn!("An actor stopped unexpectedly: {err:?}");
        }
        None => {
            warn!("No actors were registered");
        }
    }
}
