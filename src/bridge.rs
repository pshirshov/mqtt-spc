use std::collections::BTreeMap;
use std::time::Duration;

use rumqttc::{AsyncClient, EventLoop, LastWill, MqttOptions, QoS};
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::config::{Config, ModeNames};
use crate::edp::panel::{
    Area, ArmMode, PanelInfo, QUERY_AREAS, QUERY_INFO, QUERY_STATUS, QUERY_ZONES,
    SYSTEM_ALERTS, Snapshot, Zone,
};
use crate::edp::session::{BinaryOp, CommandError, LinkEvent, SessionHandle, SessionId};
use crate::edp::sia::SiaEvent;
use crate::mqtt::discovery::{self as ha, Ctx, UNMAPPED_ALERT_SLUG, ZoneControl};

const MQTT_RECONNECT_DELAY: Duration = Duration::from_secs(5);
const MQTT_KEEP_ALIVE: Duration = Duration::from_secs(30);
const MQTT_REQUEST_QUEUE_LEN: usize = 256;
const MQTT_EVENT_QUEUE_LEN: usize = 16;

enum MqttEvent {
    Connected,
    Disconnected,
    HomeAssistantOnline,
    AreaCommand { area_id: u8, payload: String },
    AreaModeCommand { area_id: u8, payload: String },
    ZoneCommand { zone_id: u8, control: ZoneControl, payload: String },
}

/// Home Assistant alarm_control_panel command payloads.
fn alarm_panel_mode(payload: &str) -> Option<ArmMode> {
    match payload {
        "DISARM" => Some(ArmMode::Unset),
        "ARM_HOME" => Some(ArmMode::PartSetA),
        "ARM_NIGHT" => Some(ArmMode::PartSetB),
        "ARM_AWAY" => Some(ArmMode::FullSet),
        _ => None,
    }
}

fn arm_op(mode: ArmMode) -> BinaryOp {
    match mode {
        ArmMode::Unset => BinaryOp::AreaUnset,
        ArmMode::PartSetA => BinaryOp::AreaPartSetA,
        ArmMode::PartSetB => BinaryOp::AreaPartSetB,
        ArmMode::FullSet => BinaryOp::AreaFullSet,
    }
}

/// Select state: the panel's name for the mode.
fn area_mode_label<'a>(area: &Area, names: &'a ModeNames) -> &'a str {
    match area.mode {
        Some(mode) => names.label(mode),
        None => "None",
    }
}

/// Home Assistant switch command payloads.
fn zone_op(control: ZoneControl, payload: &str) -> Option<BinaryOp> {
    match (control, payload) {
        (ZoneControl::Inhibit, "ON") => Some(BinaryOp::ZoneInhibit),
        (ZoneControl::Inhibit, "OFF") => Some(BinaryOp::ZoneDeinhibit),
        (ZoneControl::Isolate, "ON") => Some(BinaryOp::ZoneIsolate),
        (ZoneControl::Isolate, "OFF") => Some(BinaryOp::ZoneDeisolate),
        _ => None,
    }
}

fn on_off(on: bool) -> &'static str {
    if on { "ON" } else { "OFF" }
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
        if effect.refresh_alerts {
            match link.session.xml_query(QUERY_STATUS).await {
                Ok(reply) => {
                    if let Err(e) = loaded.snapshot.apply_status(&reply) {
                        warn!("Bad status reply after event {}: {e}", ev.code);
                    }
                }
                Err(e) => warn!("Status read after event {} failed: {e}", ev.code),
            }
        }
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
        if effect.refresh_alerts {
            self.publish_alerts().await;
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
            let status = link.session.xml_query(QUERY_STATUS).await?;
            loaded.snapshot.apply_areas(&areas);
            loaded.snapshot.apply_zones(&zones);
            loaded.snapshot.apply_status(&status).map_err(CommandError::Protocol)?;
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
                    format!("{prefix}/area/+/mode/set"),
                    format!("{prefix}/zone/+/+/set"),
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
                let mode = alarm_panel_mode(&payload);
                self.area_command(area_id, mode, &payload).await;
            }
            MqttEvent::AreaModeCommand { area_id, payload } => {
                let mode = self.config.mode_names.mode(&payload);
                self.area_command(area_id, mode, &payload).await;
            }
            MqttEvent::ZoneCommand { zone_id, control, payload } => {
                let Some(op) = zone_op(control, &payload) else {
                    warn!("Unsupported {control:?} command {payload:?} for zone {zone_id}");
                    return;
                };
                self.command(op, zone_id, &format!("Zone {zone_id} {control:?} {payload}")).await;
                self.reread(QUERY_ZONES).await;
                self.publish_zone(u32::from(zone_id)).await;
            }
        }
    }

    async fn area_command(&mut self, area_id: u8, mode: Option<ArmMode>, payload: &str) {
        let Some(mode) = mode else {
            warn!("Unsupported command {payload:?} for area {area_id}");
            return;
        };
        self.command(arm_op(mode), area_id, &format!("Area {area_id} {payload}")).await;
        self.reread(QUERY_AREAS).await;
        self.publish_area(area_id).await;
    }

    /// Send a binary command; a failure is logged and surfaced on the event
    /// topic. The caller re-reads and re-publishes the state either way, so
    /// Home Assistant drops any optimistic view.
    async fn command(&mut self, op: BinaryOp, target: u8, what: &str) {
        let Some(link) = &self.link else {
            warn!("{what} dropped: panel not connected");
            return;
        };
        info!("{what} -> {op:?}");
        if let Err(e) = link.session.clone().binary_command(op, target).await {
            error!("{what} failed: {e}");
            let text = format!("{what} failed: {e}");
            self.publish(
                format!("{}/event", self.config.topic_prefix),
                false,
                json!({ "text": text }).to_string(),
            )
            .await;
        }
    }

    async fn reread(&mut self, query: &str) {
        let Some(Link { session, loaded: Some(loaded) }) = &mut self.link else { return };
        match session.xml_query(query).await {
            Ok(reply) if query == QUERY_AREAS => loaded.snapshot.apply_areas(&reply),
            Ok(reply) => loaded.snapshot.apply_zones(&reply),
            Err(e) => warn!("Re-read of {query} after command failed: {e}"),
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
        let desired = ha::discovery_messages(
            &loaded.snapshot,
            &ctx,
            &self.config.zone_device_class,
            &self.config.mode_names,
        );
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
        self.publish_alerts().await;
    }

    async fn publish_alerts(&self) {
        let Some(loaded) = self.loaded() else { return };
        let alerts = loaded.snapshot.alerts;
        let prefix = &self.config.topic_prefix;
        for def in &SYSTEM_ALERTS {
            let topic = format!("{prefix}/system/{}", def.slug);
            self.publish(topic.clone(), true, on_off(alerts.is_active(def.bit)).into()).await;
            let attributes = json!({
                "inhibited": alerts.is_inhibited(def.bit),
                "isolated": alerts.is_isolated(def.bit),
            });
            self.publish(format!("{topic}/attributes"), true, attributes.to_string()).await;
        }
        let unmapped = alerts.unmapped_active();
        if !unmapped.is_empty() {
            warn!("Active system alert bits without a known name: {unmapped:?}");
        }
        let topic = format!("{prefix}/system/{UNMAPPED_ALERT_SLUG}");
        self.publish(topic.clone(), true, on_off(!unmapped.is_empty()).into()).await;
        self.publish(format!("{topic}/attributes"), true, json!({ "bits": unmapped }).to_string())
            .await;
    }

    async fn publish_area(&self, id: u8) {
        let Some(area) = self.loaded().and_then(|l| l.snapshot.areas.get(&id)) else { return };
        let prefix = &self.config.topic_prefix;
        self.publish(format!("{prefix}/area/{id}/state"), true, area_state(area).into()).await;
        let label = area_mode_label(area, &self.config.mode_names);
        self.publish(format!("{prefix}/area/{id}/mode"), true, label.into()).await;
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
            "status": zone.status.to_string(),
        });
        self.publish(format!("{prefix}/zone/{id}/attributes"), true, attributes.to_string())
            .await;
        for control in ZoneControl::ALL {
            if control.available(zone) {
                self.publish(
                    format!("{prefix}/zone/{id}/{}", control.slug()),
                    true,
                    on_off(control.is_on(zone)).into(),
                )
                .await;
            }
        }
    }
}

async fn drive_eventloop(
    mut eventloop: EventLoop,
    tx: mpsc::Sender<MqttEvent>,
    prefix: String,
    discovery_prefix: String,
) {
    let area_prefix = format!("{prefix}/area/");
    let zone_prefix = format!("{prefix}/zone/");
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
                    parse_area_command(id, payload)
                } else if let Some(rest) = p
                    .topic
                    .strip_prefix(&zone_prefix)
                    .and_then(|rest| rest.strip_suffix("/set"))
                {
                    parse_zone_command(rest, payload)
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

/// `<zone id>/<inhibit|isolate>` from a `.../zone/+/+/set` topic.
fn parse_zone_command(rest: &str, payload: String) -> Option<MqttEvent> {
    let (id, control) = rest.split_once('/')?;
    let control = ZoneControl::ALL.into_iter().find(|c| c.slug() == control);
    match (id.parse::<u8>(), control) {
        (Ok(zone_id), Some(control)) => Some(MqttEvent::ZoneCommand { zone_id, control, payload }),
        _ => {
            warn!("Ignoring zone command on unrecognised topic suffix {rest:?}");
            None
        }
    }
}

/// `<area id>` (alarm panel) or `<area id>/mode` (select) from a `.../area/…/set` topic.
fn parse_area_command(rest: &str, payload: String) -> Option<MqttEvent> {
    let (id, is_mode) = match rest.strip_suffix("/mode") {
        Some(id) => (id, true),
        None => (rest, false),
    };
    match id.parse::<u8>() {
        Ok(area_id) if is_mode => Some(MqttEvent::AreaModeCommand { area_id, payload }),
        Ok(area_id) => Some(MqttEvent::AreaCommand { area_id, payload }),
        Err(_) => {
            warn!("Ignoring area command on unrecognised topic suffix {rest:?}");
            None
        }
    }
}
