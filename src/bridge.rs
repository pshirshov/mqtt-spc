use std::collections::BTreeMap;
use std::time::Duration;

use rumqttc::{AsyncClient, EventLoop, LastWill, MqttOptions, QoS};
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::edp::panel::{
    Area, ArmMode, PanelInfo, QUERY_AREAS, QUERY_INFO, QUERY_ZONES, Snapshot, Zone,
};
use crate::edp::session::{AreaOp, CommandError, LinkEvent, SessionHandle, SessionId};
use crate::edp::sia::SiaEvent;
use crate::mqtt::discovery::{self as ha, Ctx};

const MQTT_RECONNECT_DELAY: Duration = Duration::from_secs(5);
const MQTT_KEEP_ALIVE: Duration = Duration::from_secs(30);
const MQTT_REQUEST_QUEUE_LEN: usize = 256;
const MQTT_EVENT_QUEUE_LEN: usize = 16;

enum MqttEvent {
    Connected,
    Disconnected,
    HomeAssistantOnline,
    AreaCommand { area_id: u8, payload: String },
}

/// Home Assistant alarm_control_panel command payloads.
fn area_op(payload: &str) -> Option<AreaOp> {
    match payload {
        "DISARM" => Some(AreaOp::Unset),
        "ARM_HOME" => Some(AreaOp::PartSetA),
        "ARM_NIGHT" => Some(AreaOp::PartSetB),
        "ARM_AWAY" => Some(AreaOp::FullSet),
        _ => None,
    }
}

/// Home Assistant alarm_control_panel state payload.
fn area_state(area: &Area) -> &'static str {
    if area.triggered {
        return "triggered";
    }
    match area.mode {
        Some(ArmMode::Unset) => "disarmed",
        Some(ArmMode::PartSetA) => "armed_home",
        Some(ArmMode::PartSetB) => "armed_night",
        Some(ArmMode::FullSet) => "armed_away",
        None => "None",
    }
}

fn zone_state(zone: &Zone) -> &'static str {
    match zone.input.is_open() {
        Some(true) => "ON",
        Some(false) => "OFF",
        None => "None",
    }
}

struct Loaded {
    info: PanelInfo,
    snapshot: Snapshot,
}

struct Link {
    session: SessionHandle,
    /// `None` until the initial info/area/zone read succeeds.
    loaded: Option<Loaded>,
}

struct Bridge<'a> {
    config: &'a Config,
    mqtt: AsyncClient,
    mqtt_up: bool,
    link: Option<Link>,
    /// Discovery configs published during the current MQTT connection.
    discovery: BTreeMap<String, String>,
}

pub async fn run(config: &Config, mut link_rx: mpsc::Receiver<LinkEvent>) {
    let prefix = &config.topic_prefix;
    let client_id = format!("spc_mqtt_{}", std::process::id());
    let mut opts = MqttOptions::new(&client_id, &config.mqtt_host, config.mqtt_port);
    opts.set_keep_alive(MQTT_KEEP_ALIVE);
    if let Some(creds) = &config.mqtt_creds {
        opts.set_credentials(&creds.login, &creds.password);
    }
    opts.set_last_will(LastWill::new(
        format!("{prefix}/status"),
        "offline".as_bytes().to_vec(),
        QoS::AtLeastOnce,
        true,
    ));
    let (mqtt, eventloop) = AsyncClient::new(opts, MQTT_REQUEST_QUEUE_LEN);

    let (mqtt_tx, mut mqtt_rx) = mpsc::channel(MQTT_EVENT_QUEUE_LEN);
    tokio::spawn(drive_eventloop(
        eventloop,
        mqtt_tx,
        prefix.clone(),
        config.discovery_prefix.clone(),
    ));

    let mut bridge = Bridge {
        config,
        mqtt,
        mqtt_up: false,
        link: None,
        discovery: BTreeMap::new(),
    };
    let mut refresh = tokio::time::interval(config.refresh_interval);
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            ev = link_rx.recv() => match ev {
                Some(ev) => bridge.on_link_event(ev).await,
                None => {
                    error!("EDP receiver stopped");
                    return;
                }
            },
            ev = mqtt_rx.recv() => match ev {
                Some(ev) => bridge.on_mqtt_event(ev).await,
                None => {
                    error!("MQTT event loop stopped");
                    return;
                }
            },
            _ = refresh.tick() => bridge.refresh().await,
        }
    }
}

impl Bridge<'_> {
    fn is_current(&self, id: SessionId) -> bool {
        self.link.as_ref().is_some_and(|l| l.session.id() == id)
    }

    async fn on_link_event(&mut self, ev: LinkEvent) {
        match ev {
            LinkEvent::Ready(session) => {
                info!("Panel session {} ready", session.id());
                self.link = Some(Link { session, loaded: None });
                self.refresh().await;
            }
            LinkEvent::Closed(id) if self.is_current(id) => {
                warn!("Panel session {id} closed; marking entities unavailable");
                self.link = None;
                self.publish_status().await;
            }
            LinkEvent::Closed(_) => {}
            LinkEvent::Sia(id, ev) if self.is_current(id) => self.on_sia(ev).await,
            LinkEvent::Sia(id, ev) => warn!("Ignoring event from stale session {id}: {ev:?}"),
        }
    }

    async fn on_sia(&mut self, ev: SiaEvent) {
        info!("Panel event {} [{}] {}", ev.code, ev.address, ev.description);
        let payload = json!({
            "text": format!("{} {}", ev.code, ev.description),
            "code": ev.code,
            "address": ev.address,
            "description": ev.description,
            "panel_time": ev.panel_time(),
        });
        self.publish(format!("{}/event", self.config.topic_prefix), false, payload.to_string())
            .await;

        let Some(link) = &mut self.link else { return };
        let Some(loaded) = &mut link.loaded else { return };
        let effect = loaded.snapshot.apply_event(&ev);
        let mut areas = effect.areas;
        if effect.refresh_areas {
            match link.session.xml_query(QUERY_AREAS).await {
                Ok(reply) => {
                    loaded.snapshot.apply_areas(&reply);
                    areas = loaded.snapshot.areas.keys().copied().collect();
                }
                Err(e) => warn!("Area read after event {} failed: {e}", ev.code),
            }
        }
        self.publish_discovery().await;
        for id in effect.zones {
            self.publish_zone(id).await;
        }
        for id in areas {
            self.publish_area(id).await;
        }
    }

    /// Initial load for a fresh session, or periodic reconciliation.
    async fn refresh(&mut self) {
        let Some(link) = &mut self.link else { return };
        let result: Result<(), CommandError> = async {
            if link.loaded.is_none() {
                let reply = link.session.xml_query(QUERY_INFO).await?;
                let info = PanelInfo::from_reply(&reply).map_err(CommandError::Protocol)?;
                info!("Panel {} S/N {} firmware {}", info.model, info.serial, info.firmware);
                link.loaded = Some(Loaded { info, snapshot: Snapshot::default() });
            }
            let loaded = link.loaded.as_mut().expect("loaded above");
            let areas = link.session.xml_query(QUERY_AREAS).await?;
            let zones = link.session.xml_query(QUERY_ZONES).await?;
            loaded.snapshot.apply_areas(&areas);
            loaded.snapshot.apply_zones(&zones);
            Ok(())
        }
        .await;
        match result {
            Ok(()) => self.publish_all().await,
            Err(e) => warn!("Panel refresh failed: {e}"),
        }
    }

    async fn on_mqtt_event(&mut self, ev: MqttEvent) {
        match ev {
            MqttEvent::Connected => {
                info!("MQTT connected to {}:{}", self.config.mqtt_host, self.config.mqtt_port);
                self.mqtt_up = true;
                self.discovery.clear();
                let prefix = &self.config.topic_prefix;
                for topic in [
                    format!("{prefix}/area/+/set"),
                    format!("{}/status", self.config.discovery_prefix),
                ] {
                    if let Err(e) = self.mqtt.subscribe(topic, QoS::AtLeastOnce).await {
                        warn!("MQTT subscribe failed: {e}");
                    }
                }
                self.publish_all().await;
            }
            MqttEvent::Disconnected => self.mqtt_up = false,
            MqttEvent::HomeAssistantOnline => {
                info!("Home Assistant birth message received, re-publishing discovery");
                self.discovery.clear();
                self.publish_all().await;
            }
            MqttEvent::AreaCommand { area_id, payload } => {
                self.area_command(area_id, &payload).await;
            }
        }
    }

    async fn area_command(&mut self, area_id: u8, payload: &str) {
        let Some(op) = area_op(payload) else {
            warn!("Unsupported command {payload:?} for area {area_id}");
            return;
        };
        let Some(link) = &self.link else {
            warn!("Area {area_id} command {payload} dropped: panel not connected");
            return;
        };
        info!("Area {area_id}: {payload} -> {op:?}");
        let session = link.session.clone();
        match session.area_command(op, area_id).await {
            Ok(()) => {
                if let Some(Link { session, loaded: Some(loaded) }) = &mut self.link {
                    match session.xml_query(QUERY_AREAS).await {
                        Ok(reply) => loaded.snapshot.apply_areas(&reply),
                        Err(e) => warn!("Area read after command failed: {e}"),
                    }
                }
                self.publish_area(area_id).await;
            }
            Err(e) => {
                error!("Area {area_id} {payload} failed: {e}");
                let text = format!("Area {area_id} {payload} failed: {e}");
                self.publish(
                    format!("{}/event", self.config.topic_prefix),
                    false,
                    json!({ "text": text }).to_string(),
                )
                .await;
                // Re-assert the actual state so HA drops its optimistic view.
                self.publish_area(area_id).await;
            }
        }
    }

    async fn publish(&self, topic: String, retain: bool, payload: String) {
        if !self.mqtt_up {
            return;
        }
        if let Err(e) = self.mqtt.publish(topic, QoS::AtLeastOnce, retain, payload).await {
            warn!("MQTT publish failed: {e}");
        }
    }

    async fn publish_status(&self) {
        let online = self.link.as_ref().is_some_and(|l| l.loaded.is_some());
        let status = if online { "online" } else { "offline" };
        self.publish(format!("{}/status", self.config.topic_prefix), true, status.into())
            .await;
    }

    fn loaded(&self) -> Option<&Loaded> {
        self.link.as_ref().and_then(|l| l.loaded.as_ref())
    }

    /// Publish discovery configs that changed since the last publish, and
    /// delete those of entities that disappeared from the panel.
    async fn publish_discovery(&mut self) {
        let Some(loaded) = self.loaded() else { return };
        if !self.mqtt_up {
            return;
        }
        let ctx = Ctx {
            info: &loaded.info,
            topic_prefix: &self.config.topic_prefix,
            discovery_prefix: &self.config.discovery_prefix,
        };
        let desired = ha::discovery_messages(&loaded.snapshot, &ctx, &self.config.zone_device_class);
        let removed: Vec<String> =
            self.discovery.keys().filter(|t| !desired.contains_key(*t)).cloned().collect();
        for topic in removed {
            info!("HA discovery: removing {topic}");
            self.publish(topic.clone(), true, String::new()).await;
            self.discovery.remove(&topic);
        }
        for (topic, payload) in desired {
            if self.discovery.get(&topic) != Some(&payload) {
                self.publish(topic.clone(), true, payload.clone()).await;
                self.discovery.insert(topic, payload);
            }
        }
    }

    async fn publish_all(&mut self) {
        self.publish_discovery().await;
        self.publish_status().await;
        let Some(loaded) = self.loaded() else { return };
        let areas: Vec<u8> = loaded.snapshot.areas.keys().copied().collect();
        let zones: Vec<u32> = loaded.snapshot.zones.keys().copied().collect();
        for id in areas {
            self.publish_area(id).await;
        }
        for id in zones {
            self.publish_zone(id).await;
        }
    }

    async fn publish_area(&self, id: u8) {
        let Some(area) = self.loaded().and_then(|l| l.snapshot.areas.get(&id)) else { return };
        let prefix = &self.config.topic_prefix;
        self.publish(format!("{prefix}/area/{id}/state"), true, area_state(area).into()).await;
    }

    async fn publish_zone(&self, id: u32) {
        let Some(zone) = self.loaded().and_then(|l| l.snapshot.zones.get(&id)) else { return };
        let prefix = &self.config.topic_prefix;
        self.publish(format!("{prefix}/zone/{id}/state"), true, zone_state(zone).into()).await;
        let attributes = json!({
            "zone_name": zone.name,
            "zone_type": zone.zone_type.map(|t| t.to_string()),
            "area_id": zone.area_id,
            "input": zone.input.to_string(),
        });
        self.publish(format!("{prefix}/zone/{id}/attributes"), true, attributes.to_string())
            .await;
    }
}

async fn drive_eventloop(
    mut eventloop: EventLoop,
    tx: mpsc::Sender<MqttEvent>,
    prefix: String,
    discovery_prefix: String,
) {
    let area_prefix = format!("{prefix}/area/");
    let ha_status_topic = format!("{discovery_prefix}/status");

    loop {
        let ev = match eventloop.poll().await {
            Ok(rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(_))) => Some(MqttEvent::Connected),
            Ok(rumqttc::Event::Incoming(rumqttc::Packet::Publish(p))) => {
                let payload = String::from_utf8_lossy(&p.payload).to_string();
                if p.topic == ha_status_topic {
                    (payload == "online").then_some(MqttEvent::HomeAssistantOnline)
                } else if let Some(id) = p
                    .topic
                    .strip_prefix(&area_prefix)
                    .and_then(|rest| rest.strip_suffix("/set"))
                {
                    match id.parse::<u8>() {
                        Ok(area_id) => Some(MqttEvent::AreaCommand { area_id, payload }),
                        Err(_) => {
                            warn!("Ignoring command for invalid area {id:?}");
                            None
                        }
                    }
                } else {
                    None
                }
            }
            Ok(_) => None,
            Err(e) => {
                error!("MQTT error: {e} — reconnecting in {MQTT_RECONNECT_DELAY:?}");
                if tx.send(MqttEvent::Disconnected).await.is_err() {
                    return;
                }
                tokio::time::sleep(MQTT_RECONNECT_DELAY).await;
                None
            }
        };
        if let Some(ev) = ev
            && tx.send(ev).await.is_err() {
                return;
            }
    }
}
