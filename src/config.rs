use clap::Parser;
use serde::Deserialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use crate::edp::panel::ArmMode;
use crate::edp::session::ReceiverConfig;
use crate::edp::wire::EdpKey;

#[derive(Debug, Parser)]
#[command(about = "SPC alarm panel (EDP) to MQTT bridge for Home Assistant")]
pub struct Args {
    /// Address to accept the panel's EDP connection on
    #[arg(long, default_value = "0.0.0.0:50000")]
    pub listen: SocketAddr,

    /// EDP receiver ID, as configured for this receiver on the panel
    #[arg(long)]
    pub receiver_id: u32,

    /// File holding the receiver's 32-hex-digit EDP AES key (omit for unencrypted EDP)
    #[arg(long)]
    pub edp_key_file: Option<String>,

    /// Drop the panel connection after this many seconds without traffic
    #[arg(long, default_value_t = 120)]
    pub idle_timeout: u64,

    /// Full area/zone re-read interval in seconds (covers missed or unreported events)
    #[arg(long, default_value_t = 30)]
    pub refresh_interval: u64,

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

    /// Panel's name for part set A (its web UI shows it on the set buttons)
    #[arg(long, default_value = "Part Set A")]
    pub part_set_a_name: String,

    /// Panel's name for part set B
    #[arg(long, default_value = "Part Set B")]
    pub part_set_b_name: String,

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

/// Display labels for the arm modes. EDP reports only the mode number; the
/// panel's part-set names are not available over it, so they are configured.
#[derive(Debug, Clone)]
pub struct ModeNames {
    part_set_a: String,
    part_set_b: String,
}

impl ModeNames {
    const UNSET: &str = "Unset";
    const FULL_SET: &str = "Fullset";
    pub const ORDER: [ArmMode; 4] =
        [ArmMode::Unset, ArmMode::PartSetA, ArmMode::PartSetB, ArmMode::FullSet];

    pub fn label(&self, mode: ArmMode) -> &str {
        match mode {
            ArmMode::Unset => Self::UNSET,
            ArmMode::PartSetA => &self.part_set_a,
            ArmMode::PartSetB => &self.part_set_b,
            ArmMode::FullSet => Self::FULL_SET,
        }
    }

    fn from_args(part_set_a: String, part_set_b: String) -> Self {
        let names = Self { part_set_a, part_set_b };
        let labels: Vec<&str> = Self::ORDER.iter().map(|&m| names.label(m)).collect();
        for (i, label) in labels.iter().enumerate() {
            assert!(
                !label.is_empty() && !labels[..i].contains(label),
                "arm mode names must be non-empty and distinct: {labels:?}"
            );
        }
        names
    }

    pub fn mode(&self, label: &str) -> Option<ArmMode> {
        Self::ORDER.into_iter().find(|&m| self.label(m) == label)
    }
}

#[derive(Debug)]
pub struct Config {
    pub receiver: ReceiverConfig,
    pub refresh_interval: Duration,
    pub mqtt_host: String,
    pub mqtt_port: u16,
    pub mqtt_creds: Option<Credentials>,
    pub topic_prefix: String,
    pub discovery_prefix: String,
    pub zone_device_class: HashMap<u32, String>,
    pub mode_names: ModeNames,
}

impl Config {
    pub fn from_args(args: Args) -> Self {
        let key = args.edp_key_file.as_ref().map(|path| {
            let hex = std::fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("failed to read EDP key {path}: {e}"));
            EdpKey::from_hex(&hex).unwrap_or_else(|e| panic!("invalid EDP key in {path}: {e}"))
        });

        let mqtt_creds_path = Path::new(&args.mqtt_creds);
        let mqtt_creds = if mqtt_creds_path.is_file() {
            Some(Credentials::load(mqtt_creds_path))
        } else {
            None
        };

        Config {
            receiver: ReceiverConfig {
                listen: args.listen,
                receiver_id: args.receiver_id,
                key,
                idle_timeout: Duration::from_secs(args.idle_timeout),
            },
            refresh_interval: Duration::from_secs(args.refresh_interval),
            mqtt_host: args.mqtt_host,
            mqtt_port: args.mqtt_port,
            mqtt_creds,
            topic_prefix: args.topic_prefix,
            discovery_prefix: args.discovery_prefix,
            zone_device_class: args.zone_classes.into_iter().collect(),
            mode_names: ModeNames::from_args(args.part_set_a_name, args.part_set_b_name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_names_round_trip() {
        let names = ModeNames::from_args("Ground Floor".into(), "All but access".into());
        for mode in ModeNames::ORDER {
            assert_eq!(names.mode(names.label(mode)), Some(mode));
        }
        assert_eq!(names.label(ArmMode::PartSetA), "Ground Floor");
        assert_eq!(names.mode("ARM_AWAY"), None);
    }

    #[test]
    #[should_panic(expected = "distinct")]
    fn mode_names_must_be_distinct() {
        ModeNames::from_args("Fullset".into(), "B".into());
    }
}
