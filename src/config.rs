use clap::Parser;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Parser)]
#[command(about = "SPC alarm panel to MQTT bridge for Home Assistant")]
pub struct Args {
    /// SPC panel base URL
    #[arg(long)]
    pub spc_url: String,

    /// Path to SPC credentials JSON ({"login": "...", "password": "..."})
    #[arg(long, default_value = "creds.json")]
    pub spc_creds: String,

    /// MQTT broker host
    #[arg(long)]
    pub mqtt_host: String,

    /// MQTT broker port
    #[arg(long, default_value_t = 1883)]
    pub mqtt_port: u16,

    /// Path to MQTT credentials JSON ({"login": "...", "password": "..."})
    #[arg(long, default_value = "mqtt-creds.json")]
    pub mqtt_creds: String,

    /// MQTT topic prefix
    #[arg(long, default_value = "spc")]
    pub topic_prefix: String,

    /// Home Assistant discovery prefix
    #[arg(long, default_value = "homeassistant")]
    pub discovery_prefix: String,

    /// Poll interval in seconds
    #[arg(long, default_value_t = 5)]
    pub poll_interval: u64,

    /// Zone device class overrides (e.g. 1=door 2=motion)
    #[arg(long = "zone-class", value_parser = parse_zone_class)]
    pub zone_classes: Vec<(u32, String)>,
}

fn parse_zone_class(s: &str) -> Result<(u32, String), String> {
    let (id, class) = s
        .split_once('=')
        .ok_or_else(|| format!("expected ID=CLASS, got {s:?}"))?;
    let id: u32 = id.parse().map_err(|e| format!("invalid zone ID: {e}"))?;
    Ok((id, class.to_string()))
}

#[derive(Debug, Deserialize)]
pub struct Credentials {
    pub login: String,
    pub password: String,
}

impl Credentials {
    pub fn load(path: &Path) -> Self {
        let contents = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("failed to read credentials {}: {e}", path.display()));
        serde_json::from_str(&contents)
            .unwrap_or_else(|e| panic!("failed to parse credentials {}: {e}", path.display()))
    }
}

#[derive(Debug)]
pub struct Config {
    pub spc_url: String,
    pub poll_interval_secs: u64,
    pub mqtt_host: String,
    pub mqtt_port: u16,
    pub mqtt_creds: Option<Credentials>,
    pub topic_prefix: String,
    pub discovery_prefix: String,
    pub zone_device_class: HashMap<u32, String>,
}

impl Config {
    pub fn from_args(args: Args) -> Self {
        let spc_creds = Path::new(&args.spc_creds);
        assert!(spc_creds.is_file(), "SPC credentials not found: {}", args.spc_creds);

        let mqtt_creds_path = Path::new(&args.mqtt_creds);
        let mqtt_creds = if mqtt_creds_path.is_file() {
            Some(Credentials::load(mqtt_creds_path))
        } else {
            None
        };

        Config {
            spc_url: args.spc_url,
            poll_interval_secs: args.poll_interval,
            mqtt_host: args.mqtt_host,
            mqtt_port: args.mqtt_port,
            mqtt_creds,
            topic_prefix: args.topic_prefix,
            discovery_prefix: args.discovery_prefix,
            zone_device_class: args.zone_classes.into_iter().collect(),
        }
    }
}
