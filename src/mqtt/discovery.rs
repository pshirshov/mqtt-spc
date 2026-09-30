use std::collections::{BTreeMap, HashMap};

use serde_json::{json, Value};

use crate::edp::panel::{Area, PanelInfo, Snapshot, Zone};

/// Panel identity, passed through to all discovery payloads.
pub struct Ctx<'a> {
    pub info: &'a PanelInfo,
    pub topic_prefix: &'a str,
    pub discovery_prefix: &'a str,
}

/// Discovery topics of entities published by the web-UI-based bridge that
/// this version no longer provides; an empty retained payload removes them.
const LEGACY_ZONE_BUTTONS: [&str; 2] = ["inhibit", "isolate"];

/// Retained discovery configs for the current snapshot: topic -> payload.
/// An empty payload deletes the entity in Home Assistant.
pub fn discovery_messages(
    snapshot: &Snapshot,
    ctx: &Ctx,
    zone_classes: &HashMap<u32, String>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    out.insert(event_sensor_discovery_topic(ctx), event_sensor_discovery_payload(ctx));
    for area in snapshot.areas.values() {
        out.insert(area_discovery_topic(area, ctx), area_discovery_payload(area, ctx));
        out.insert(legacy_topic(ctx, "select", &format!("area_{}", area.id)), String::new());
    }
    for zone in snapshot.zones.values() {
        let class = zone_classes
            .get(&zone.id)
            .map(String::as_str)
            .or_else(|| zone.zone_type.map(|t| t.device_class()))
            .unwrap_or("opening");
        out.insert(zone_discovery_topic(zone, ctx), zone_discovery_payload(zone, class, ctx));
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

// --- Areas (alarm_control_panel) ---

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
