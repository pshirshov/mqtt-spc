use std::collections::HashSet;
use std::time::Duration;

use rumqttc::{AsyncClient, EventLoop, LastWill, MqttOptions, QoS};
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::model::{Area, PanelState, SystemAlert, Zone};
use crate::mqtt::discovery::{self as ha, Ctx};
use crate::spc::client::SpcClient;
use crate::spc::parser;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

enum Command {
    Area { area_id: u32, label: String },
    Zone { zone_id: u32, action: String },
    Alert { sensor_id: String, action: String },
}

pub async fn run(config: &Config, mut spc: SpcClient) {
    let mut state = PanelState::new();

    // Auto-detect panel info from first page fetch
    match spc.fetch_page("system_summary").await {
        Ok(html) => {
            if let Some(info) = parser::parse_panel_info(&html) {
                info!("Detected panel: {} S/N {}", info.name, info.serial);
                state.name = info.name;
                state.serial = info.serial;
            }
        }
        Err(e) => warn!("Initial panel detection failed: {e}"),
    }

    assert!(!state.serial.is_empty(), "Failed to detect panel serial number — check SPC URL and credentials");

    loop {
        let mut discovered: HashSet<String> = HashSet::new();

        let client_id = format!("spc_mqtt_{}_{}", state.serial, std::process::id());
        let mut opts = MqttOptions::new(&client_id, &config.mqtt_host, config.mqtt_port);
        opts.set_keep_alive(Duration::from_secs(30));
        if let Some(creds) = &config.mqtt_creds {
            opts.set_credentials(&creds.login, &creds.password);
        }
        let prefix = &config.topic_prefix;
        opts.set_last_will(LastWill::new(
            format!("{prefix}/status"),
            "offline".as_bytes().to_vec(),
            QoS::AtLeastOnce,
            true,
        ));

        let (client, eventloop) = AsyncClient::new(opts, 256);

        match run_session(
            config,
            &client,
            eventloop,
            &mut spc,
            &mut state,
            &mut discovered,
        )
        .await
        {
            Ok(()) => info!("MQTT session ended"),
            Err(e) => {
                error!("MQTT error: {e} — reconnecting in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

fn make_ctx<'a>(name: &'a str, serial: &'a str, config: &'a Config) -> Ctx<'a> {
    Ctx {
        name,
        serial,
        topic_prefix: &config.topic_prefix,
        discovery_prefix: &config.discovery_prefix,
    }
}

async fn run_session(
    config: &Config,
    client: &AsyncClient,
    mut eventloop: EventLoop,
    spc: &mut SpcClient,
    state: &mut PanelState,
    discovered: &mut HashSet<String>,
) -> Result<(), BoxError> {
    loop {
        if let rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(_)) = eventloop.poll().await? {
            break;
        }
    }

    let prefix = &config.topic_prefix;
    info!("MQTT connected to {}:{}", config.mqtt_host, config.mqtt_port);

    client
        .publish(format!("{prefix}/status"), QoS::AtLeastOnce, true, "online")
        .await?;

    let ctx = make_ctx(&state.name, &state.serial, config);
    client
        .publish(
            ha::event_sensor_discovery_topic(&ctx),
            QoS::AtLeastOnce,
            true,
            ha::event_sensor_discovery_payload(&ctx),
        )
        .await?;

    client
        .subscribe(format!("{prefix}/area/+/set"), QoS::AtLeastOnce)
        .await?;
    client
        .subscribe(format!("{prefix}/zone/+/action"), QoS::AtLeastOnce)
        .await?;
    client
        .subscribe(format!("{prefix}/alert/+/action"), QoS::AtLeastOnce)
        .await?;

    let (cmd_tx, mut cmd_rx) = mpsc::channel::<Command>(16);
    let prefix_owned = prefix.to_string();
    let eventloop_handle = tokio::spawn(async move {
        drive_eventloop(eventloop, cmd_tx, &prefix_owned).await;
    });

    if let Err(e) = poll_and_publish(config, client, spc, state, discovered).await {
        warn!("Initial poll failed: {e}");
    }

    let poll_interval = Duration::from_secs(config.poll_interval_secs);
    let mut poll_timer = tokio::time::interval(poll_interval);
    poll_timer.tick().await;

    loop {
        tokio::select! {
            _ = poll_timer.tick() => {
                if let Err(e) = poll_and_publish(config, client, spc, state, discovered).await {
                    warn!("Poll failed: {e}");
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(cmd) => {
                        handle_command(config, client, spc, state, discovered, cmd).await;
                    }
                    None => {
                        eventloop_handle.abort();
                        return Err("MQTT connection lost".into());
                    }
                }
            }
        }
    }
}

async fn drive_eventloop(
    mut eventloop: EventLoop,
    cmd_tx: mpsc::Sender<Command>,
    prefix: &str,
) {
    let area_prefix = format!("{prefix}/area/");
    let zone_prefix = format!("{prefix}/zone/");
    let alert_prefix = format!("{prefix}/alert/");

    loop {
        match eventloop.poll().await {
            Ok(rumqttc::Event::Incoming(rumqttc::Packet::Publish(p))) => {
                let topic = &p.topic;
                let payload = String::from_utf8_lossy(&p.payload).to_string();

                let cmd = if let Some(rest) = topic.strip_prefix(&area_prefix) {
                    rest.strip_suffix("/set").and_then(|id| {
                        id.parse::<u32>().ok().map(|area_id| {
                            info!("Command: area {area_id} = {payload}");
                            Command::Area { area_id, label: payload.clone() }
                        })
                    })
                } else if let Some(rest) = topic.strip_prefix(&zone_prefix) {
                    rest.strip_suffix("/action").and_then(|id| {
                        id.parse::<u32>().ok().map(|zone_id| {
                            info!("Command: zone {zone_id} action={payload}");
                            Command::Zone { zone_id, action: payload.clone() }
                        })
                    })
                } else if let Some(rest) = topic.strip_prefix(&alert_prefix) {
                    rest.strip_suffix("/action").map(|sensor_id| {
                        info!("Command: alert {sensor_id} action={payload}");
                        Command::Alert { sensor_id: sensor_id.to_string(), action: payload.clone() }
                    })
                } else {
                    None
                };

                if let Some(cmd) = cmd {
                    if cmd_tx.send(cmd).await.is_err() {
                        return;
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                error!("MQTT eventloop error: {e}");
                return;
            }
        }
    }
}

// --- Polling ---

async fn poll_and_publish(
    config: &Config,
    client: &AsyncClient,
    spc: &mut SpcClient,
    state: &mut PanelState,
    discovered: &mut HashSet<String>,
) -> Result<(), BoxError> {
    let areas_html = spc.fetch_page("system_summary").await?;
    let zones_html = spc.fetch_page("status_zones").await?;
    let alerts_html = spc.fetch_page("status_sysalert").await?;

    let parsed_areas = parser::parse_areas(&areas_html);
    let parsed_zones = parser::parse_zones(&zones_html);
    let parsed_alerts = parser::parse_alerts(&alerts_html);

    let prefix = &config.topic_prefix;

    // Update areas
    for pa in &parsed_areas {
        let area = state.areas.entry(pa.id).or_insert_with(|| Area::new(pa.id));
        if !pa.name.is_empty() {
            area.name.clone_from(&pa.name);
        }
        let old_state = area.state.clone();
        area.state.clone_from(&pa.state);
        area.actions = pa.actions.clone();
        area.update_options();

        let ctx = make_ctx(&state.name, &state.serial, config);
        let key = format!("area_{}", area.id);
        let options_key = format!("area_{}_opts_{}", area.id, area.select_options().join(","));
        if !discovered.contains(&key) || !discovered.contains(&options_key) {
            discovered.insert(key);
            discovered.insert(options_key);
            client
                .publish(
                    ha::area_discovery_topic(area, &ctx),
                    QoS::AtLeastOnce,
                    true,
                    ha::area_discovery_payload(area, &ctx),
                )
                .await?;
            info!("HA discovery: area {} ({}) options={:?}", area.id, area.name, area.select_options());
        }

        if area.state != old_state {
            info!("Area {} ({}) state: {old_state} -> {}", area.id, area.name, area.state);
        }
        client
            .publish(
                format!("{prefix}/area/{}/state", area.id),
                QoS::AtLeastOnce,
                true,
                area.state.as_str(),
            )
            .await?;
    }

    // Update zones
    for pz in &parsed_zones {
        let zone = state.zones.entry(pz.id).or_insert_with(|| Zone::new(pz.id));
        zone.name.clone_from(&pz.name);
        zone.area_id = pz.area_id;
        zone.zone_type.clone_from(&pz.zone_type);
        zone.input = pz.input;
        zone.status.clone_from(&pz.status);

        if zone.device_class.is_empty() {
            if let Some(dc) = config.zone_device_class.get(&zone.id) {
                zone.device_class.clone_from(dc);
            } else {
                zone.device_class = default_device_class(&zone.zone_type);
            }
        }

        let new_state = pz.zone_state();
        let old_state = zone.state;
        zone.state = new_state;

        let ctx = make_ctx(&state.name, &state.serial, config);
        ensure_zone_discovery(client, &ctx, discovered, zone).await?;

        if new_state != old_state {
            info!("Zone {} ({}) state: {old_state} -> {new_state}", zone.id, zone.name);
        }

        client
            .publish(
                format!("{prefix}/zone/{}/state", zone.id),
                QoS::AtLeastOnce,
                true,
                if zone.state.is_on() { "ON" } else { "OFF" },
            )
            .await?;

        client
            .publish(
                format!("{prefix}/zone/{}/attributes", zone.id),
                QoS::AtLeastOnce,
                true,
                json!({
                    "zone_name": zone.name,
                    "zone_type": zone.zone_type,
                    "area_id": zone.area_id,
                    "input": zone.input.to_string(),
                    "status": zone.status,
                    "state_detail": zone.state.to_string(),
                })
                .to_string(),
            )
            .await?;
    }

    // Update alerts
    let new_alerts: Vec<SystemAlert> = parsed_alerts
        .iter()
        .map(|a| SystemAlert {
            name: a.name.clone(),
            ok: a.ok,
            button_index: a.button_index,
        })
        .collect();

    if new_alerts != state.alerts || !discovered.contains("system_sensors") {
        discovered.insert("system_sensors".to_string());
        let ctx = make_ctx(&state.name, &state.serial, config);

        for alert in &new_alerts {
            let sensor_id = alert_sensor_id(&alert.name);
            let state_topic = format!("{prefix}/system/{sensor_id}");

            client
                .publish(
                    ha::system_sensor_discovery_topic(&sensor_id, &ctx),
                    QoS::AtLeastOnce,
                    true,
                    ha::system_sensor_discovery_payload(&sensor_id, &alert.name, &state_topic, &ctx),
                )
                .await?;

            client
                .publish(
                    state_topic,
                    QoS::AtLeastOnce,
                    true,
                    if alert.ok { "OFF" } else { "ON" },
                )
                .await?;

            if alert.button_index > 0 || alert.name.contains("Fault") || alert.name.contains("Tamper") {
                for action in ["inhibit", "isolate"] {
                    client
                        .publish(
                            ha::alert_button_discovery_topic(&sensor_id, action, &ctx),
                            QoS::AtLeastOnce,
                            true,
                            ha::alert_button_discovery_payload(&sensor_id, &alert.name, action, &ctx),
                        )
                        .await?;
                }
            }
        }
        state.alerts = new_alerts;
    }

    Ok(())
}

fn default_device_class(zone_type: &str) -> String {
    match zone_type {
        t if t.contains("Entry") || t.contains("Exit") => "door".into(),
        t if t.contains("Fire") => "smoke".into(),
        t if t.contains("Alarm") => "motion".into(),
        _ => "opening".into(),
    }
}

fn alert_sensor_id(name: &str) -> String {
    name.to_lowercase()
        .replace(' ', "_")
        .replace('.', "")
        .replace("controller_", "")
}

async fn handle_command(
    config: &Config,
    client: &AsyncClient,
    spc: &mut SpcClient,
    state: &mut PanelState,
    discovered: &mut HashSet<String>,
    cmd: Command,
) {
    let (button, page) = match &cmd {
        Command::Area { area_id, label } => {
            let form_name = state
                .areas
                .get(area_id)
                .and_then(|area| {
                    if area.state == *label {
                        info!("Area {area_id} already in state {label:?}, skipping");
                        return None;
                    }
                    // First check parsed action buttons (covers all cases including "all_areas")
                    if let Some(action) = area.actions.iter().find(|a| a.label == *label) {
                        return Some(action.form_name.clone());
                    }
                    // "Unset" may not appear as a button when already unset —
                    // construct the form name from the existing button naming pattern
                    if label == "Unset" {
                        // Derive pattern from existing actions: "fullset_all_areas" → "unset_all_areas"
                        if let Some(action) = area.actions.first() {
                            if let Some(suffix) = action.form_name.strip_prefix("fullset_")
                                .or_else(|| action.form_name.strip_prefix("partset_a_"))
                                .or_else(|| action.form_name.strip_prefix("partset_b_"))
                            {
                                return Some(format!("unset_{suffix}"));
                            }
                        }
                        return Some(format!("unset_area{area_id}"));
                    }
                    None
                });
            let Some(name) = form_name else {
                warn!("No action for area {area_id} label {label:?}");
                return;
            };
            (name, "system_summary")
        }
        Command::Zone { zone_id, action } => {
            (format!("{action}{zone_id}"), "status_zones")
        }
        Command::Alert { sensor_id, action } => {
            let idx = state
                .alerts
                .iter()
                .position(|a| alert_sensor_id(&a.name) == *sensor_id);
            let Some(idx) = idx else {
                warn!("No alert found for sensor_id {sensor_id:?}");
                return;
            };
            (format!("{action}{}", state.alerts[idx].button_index), "status_sysalert")
        }
    };

    match spc.post_command_to_page(page, &button).await {
        Ok(()) => {
            info!("Command sent: {button} to {page}");
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Err(e) = poll_and_publish(config, client, spc, state, discovered).await {
                warn!("Post-command poll failed: {e}");
            }
        }
        Err(e) => error!("Failed to send command {button}: {e}"),
    }
}

async fn ensure_zone_discovery(
    client: &AsyncClient,
    ctx: &Ctx<'_>,
    discovered: &mut HashSet<String>,
    zone: &Zone,
) -> Result<(), BoxError> {
    let key = format!("zone_{}", zone.id);
    if discovered.contains(&key) {
        return Ok(());
    }
    discovered.insert(key);

    client
        .publish(
            ha::zone_discovery_topic(zone, ctx),
            QoS::AtLeastOnce,
            true,
            ha::zone_discovery_payload(zone, ctx),
        )
        .await?;

    for action in ["inhibit", "isolate"] {
        client
            .publish(
                ha::zone_button_discovery_topic(zone, action, ctx),
                QoS::AtLeastOnce,
                true,
                ha::zone_button_discovery_payload(zone, action, ctx),
            )
            .await?;
    }

    info!("HA discovery: zone {} ({})", zone.id, zone.name);
    Ok(())
}
