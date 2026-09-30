use std::collections::{BTreeMap, HashMap};

use serde_json::{json, Value};

use crate::config::ModeNames;
use crate::edp::panel::{Area, PanelInfo, SYSTEM_ALERTS, Snapshot, Zone, ZoneStatus};

/// Panel identity, passed through to all discovery payloads.
pub struct Ctx<'a> {
    pub info: &'a PanelInfo,
    pub topic_prefix: &'a str,
    pub discovery_prefix: &'a str,
}

/// Zone bypass controls exposed as Home Assistant switches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneControl {
    Inhibit,
    Isolate,
}

impl ZoneControl {
    pub const ALL: [ZoneControl; 2] = [ZoneControl::Inhibit, ZoneControl::Isolate];

    pub fn slug(self) -> &'static str {
        match self {
            Self::Inhibit => "inhibit",
            Self::Isolate => "isolate",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Inhibit => "Inhibit",
            Self::Isolate => "Isolate",
        }
    }

    /// The panel reports per zone whether the operation is permitted.
    pub fn available(self, zone: &Zone) -> bool {
        match self {
            Self::Inhibit => zone.inhibit_allowed,
            Self::Isolate => zone.isolate_allowed,
        }
    }

    pub fn is_on(self, zone: &Zone) -> bool {
        match self {
            Self::Inhibit => zone.status == ZoneStatus::Inhibited,
            Self::Isolate => zone.status == ZoneStatus::Isolated,
        }
    }
}

/// Sensor for active SYSALERT bits that have no known name.
pub const UNMAPPED_ALERT_SLUG: &str = "unmapped_alert";

/// Discovery topics of entities published by the web-UI-based bridge that
/// this version no longer provides; an empty retained payload removes them.
const LEGACY_ZONE_BUTTONS: [&str; 2] = ["inhibit", "isolate"];

/// Retained discovery configs for the current snapshot: topic -> payload.
/// An empty payload deletes the entity in Home Assistant.
pub fn discovery_messages(
    snapshot: &Snapshot,
    ctx: &Ctx,
    zone_classes: &HashMap<u32, String>,
    mode_names: &ModeNames,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    out.insert(event_sensor_discovery_topic(ctx), event_sensor_discovery_payload(ctx));
    for def in &SYSTEM_ALERTS {
        out.insert(alert_discovery_topic(def.slug, ctx), alert_discovery_payload(def.slug, def.name, ctx));
    }
    out.insert(
        alert_discovery_topic(UNMAPPED_ALERT_SLUG, ctx),
        alert_discovery_payload(UNMAPPED_ALERT_SLUG, "Unmapped System Alert", ctx),
    );
    for area in snapshot.areas.values() {
        out.insert(area_discovery_topic(area, ctx), area_discovery_payload(area, ctx));
        out.insert(
            area_mode_discovery_topic(area, ctx),
            area_mode_discovery_payload(area, mode_names, ctx),
        );
        out.insert(legacy_topic(ctx, "select", &format!("area_{}", area.id)), String::new());
    }
    for zone in snapshot.zones.values() {
        let class = zone_classes
            .get(&zone.id)
            .map(String::as_str)
            .or_else(|| zone.zone_type.map(|t| t.device_class()))
            .unwrap_or("opening");
        out.insert(zone_discovery_topic(zone, ctx), zone_discovery_payload(zone, class, ctx));
        for control in ZoneControl::ALL.into_iter().filter(|c| c.available(zone)) {
            out.insert(
                zone_switch_discovery_topic(zone, control, ctx),
                zone_switch_discovery_payload(zone, control, ctx),
            );
        }
        for action in LEGACY_ZONE_BUTTONS {
            out.insert(
                legacy_topic(ctx, "button", &format!("zone_{}_{action}", zone.id)),
                String::new(),
            );
        }
    }
    out
}

fn device_info(ctx: &Ctx) -> Value {
    json!({
        "identifiers": [format!("spc_{}", ctx.info.serial)],
        "name": ctx.info.model,
        "manufacturer": "Vanderbilt",
        "model": ctx.info.model,
        "serial_number": ctx.info.serial,
        "sw_version": ctx.info.firmware,
    })
}

fn node_id(ctx: &Ctx) -> String {
    format!("spc_{}", ctx.info.serial)
}

fn availability(ctx: &Ctx) -> Value {
    json!({
        "availability_topic": format!("{}/status", ctx.topic_prefix),
        "payload_available": "online",
        "payload_not_available": "offline",
    })
}

fn merge(base: &mut Value, extra: &Value) {
    if let (Some(a), Some(b)) = (base.as_object_mut(), extra.as_object()) {
        for (k, v) in b {
            a.insert(k.clone(), v.clone());
        }
    }
}

fn legacy_topic(ctx: &Ctx, component: &str, object_id: &str) -> String {
    format!("{}/{component}/{}/{object_id}/config", ctx.discovery_prefix, node_id(ctx))
}

// --- Zones (binary_sensor) ---

fn zone_discovery_topic(zone: &Zone, ctx: &Ctx) -> String {
    format!(
        "{}/binary_sensor/{}/zone_{}/config",
        ctx.discovery_prefix,
        node_id(ctx),
        zone.id
    )
}

fn zone_discovery_payload(zone: &Zone, device_class: &str, ctx: &Ctx) -> String {
    let prefix = ctx.topic_prefix;
    let name = if zone.name.is_empty() {
        format!("Zone {}", zone.id)
    } else {
        zone.name.clone()
    };

    let mut payload = json!({
        "name": name,
        "unique_id": format!("spc_{}_zone_{}", ctx.info.serial, zone.id),
        "state_topic": format!("{prefix}/zone/{}/state", zone.id),
        "payload_on": "ON",
        "payload_off": "OFF",
        "json_attributes_topic": format!("{prefix}/zone/{}/attributes", zone.id),
        "device_class": device_class,
        "device": device_info(ctx),
    });
    merge(&mut payload, &availability(ctx));

    payload.to_string()
}

// --- Zone inhibit/isolate (switch) ---

fn zone_switch_discovery_topic(zone: &Zone, control: ZoneControl, ctx: &Ctx) -> String {
    format!(
        "{}/switch/{}/zone_{}_{}/config",
        ctx.discovery_prefix,
        node_id(ctx),
        zone.id,
        control.slug()
    )
}

fn zone_switch_discovery_payload(zone: &Zone, control: ZoneControl, ctx: &Ctx) -> String {
    let prefix = ctx.topic_prefix;
    let zone_name = if zone.name.is_empty() {
        format!("Zone {}", zone.id)
    } else {
        zone.name.clone()
    };
    let mut payload = json!({
        "name": format!("{zone_name} {}", control.label()),
        "unique_id": format!("spc_{}_zone_{}_{}", ctx.info.serial, zone.id, control.slug()),
        "state_topic": format!("{prefix}/zone/{}/{}", zone.id, control.slug()),
        "command_topic": format!("{prefix}/zone/{}/{}/set", zone.id, control.slug()),
        "payload_on": "ON",
        "payload_off": "OFF",
        "entity_category": "config",
        "icon": "mdi:shield-off-outline",
        "device": device_info(ctx),
    });
    merge(&mut payload, &availability(ctx));

    payload.to_string()
}

// --- System alerts (binary_sensor, problem) ---

fn alert_discovery_topic(slug: &str, ctx: &Ctx) -> String {
    format!("{}/binary_sensor/{}/{slug}/config", ctx.discovery_prefix, node_id(ctx))
}

fn alert_discovery_payload(slug: &str, name: &str, ctx: &Ctx) -> String {
    let prefix = ctx.topic_prefix;
    let mut payload = json!({
        "name": name,
        "unique_id": format!("spc_{}_{slug}", ctx.info.serial),
        "state_topic": format!("{prefix}/system/{slug}"),
        "json_attributes_topic": format!("{prefix}/system/{slug}/attributes"),
        "payload_on": "ON",
        "payload_off": "OFF",
        "device_class": "problem",
        "entity_category": "diagnostic",
        "device": device_info(ctx),
    });
    merge(&mut payload, &availability(ctx));

    payload.to_string()
}

// --- Areas (alarm_control_panel) ---

// --- Area mode with the panel's names (select) ---

fn area_mode_discovery_topic(area: &Area, ctx: &Ctx) -> String {
    format!("{}/select/{}/area_{}_mode/config", ctx.discovery_prefix, node_id(ctx), area.id)
}

fn area_mode_discovery_payload(area: &Area, mode_names: &ModeNames, ctx: &Ctx) -> String {
    let prefix = ctx.topic_prefix;
    let area_name = if area.name.is_empty() {
        format!("Area {}", area.id)
    } else {
        area.name.clone()
    };
    let options: Vec<&str> = ModeNames::ORDER.iter().map(|&m| mode_names.label(m)).collect();
    let mut payload = json!({
        "name": format!("{area_name} Mode"),
        "unique_id": format!("spc_{}_area_{}_mode", ctx.info.serial, area.id),
        "state_topic": format!("{prefix}/area/{}/mode", area.id),
        "command_topic": format!("{prefix}/area/{}/mode/set", area.id),
        "options": options,
        "icon": "mdi:shield-home",
        "device": device_info(ctx),
    });
    merge(&mut payload, &availability(ctx));

    payload.to_string()
}

fn area_discovery_topic(area: &Area, ctx: &Ctx) -> String {
    format!(
        "{}/alarm_control_panel/{}/area_{}/config",
        ctx.discovery_prefix,
        node_id(ctx),
        area.id
    )
}

fn area_discovery_payload(area: &Area, ctx: &Ctx) -> String {
    let prefix = ctx.topic_prefix;
    let name = if area.name.is_empty() {
        format!("Area {}", area.id)
    } else {
        area.name.clone()
    };

    // Access control is left to Home Assistant and the panel's EDP
    // receiver permissions; the panel takes no PIN over EDP.
    let mut payload = json!({
        "name": name,
        "unique_id": format!("spc_{}_area_{}", ctx.info.serial, area.id),
        "state_topic": format!("{prefix}/area/{}/state", area.id),
        "command_topic": format!("{prefix}/area/{}/set", area.id),
        "supported_features": ["arm_home", "arm_night", "arm_away"],
        "code_arm_required": false,
        "code_disarm_required": false,
        "device": device_info(ctx),
    });
    merge(&mut payload, &availability(ctx));

    payload.to_string()
}

// --- Event log sensor ---

fn event_sensor_discovery_topic(ctx: &Ctx) -> String {
    format!(
        "{}/sensor/{}/last_event/config",
        ctx.discovery_prefix,
        node_id(ctx),
    )
}

fn event_sensor_discovery_payload(ctx: &Ctx) -> String {
    let prefix = ctx.topic_prefix;
    let mut payload = json!({
        "name": "Last Event",
        "unique_id": format!("spc_{}_last_event", ctx.info.serial),
        "state_topic": format!("{prefix}/event"),
        "value_template": "{{ value_json.text[:255] }}",
        "json_attributes_topic": format!("{prefix}/event"),
        "icon": "mdi:shield-alert",
        "device": device_info(ctx),
    });
    merge(&mut payload, &availability(ctx));

    payload.to_string()
}
