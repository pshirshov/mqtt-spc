use serde_json::{json, Value};

use crate::model::{Area, Zone};

/// Panel identity, passed through to all discovery payloads.
pub struct Ctx<'a> {
    pub name: &'a str,
    pub serial: &'a str,
    pub topic_prefix: &'a str,
    pub discovery_prefix: &'a str,
}

fn device_info(ctx: &Ctx) -> Value {
    json!({
        "identifiers": [format!("spc_{}", ctx.serial)],
        "name": ctx.name,
        "manufacturer": "Vanderbilt",
        "model": ctx.name,
        "serial_number": ctx.serial,
    })
}

fn node_id(ctx: &Ctx) -> String {
    format!("spc_{}", ctx.serial)
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

// --- Zones (binary_sensor) ---

pub fn zone_discovery_topic(zone: &Zone, ctx: &Ctx) -> String {
    format!(
        "{}/binary_sensor/{}/zone_{}/config",
        ctx.discovery_prefix,
        node_id(ctx),
        zone.id
    )
}

pub fn zone_discovery_payload(zone: &Zone, ctx: &Ctx) -> String {
    let prefix = ctx.topic_prefix;
    let name = if zone.name.is_empty() {
        format!("Zone {}", zone.id)
    } else {
        zone.name.clone()
    };

    let mut payload = json!({
        "name": name,
        "unique_id": format!("spc_{}_zone_{}", ctx.serial, zone.id),
        "state_topic": format!("{prefix}/zone/{}/state", zone.id),
        "payload_on": "ON",
        "payload_off": "OFF",
        "json_attributes_topic": format!("{prefix}/zone/{}/attributes", zone.id),
        "device": device_info(ctx),
    });
    merge(&mut payload, &availability(ctx));

    if !zone.device_class.is_empty() {
        payload["device_class"] = json!(zone.device_class);
    }

    payload.to_string()
}

// --- Areas (select) ---

pub fn area_discovery_topic(area: &Area, ctx: &Ctx) -> String {
    format!(
        "{}/select/{}/area_{}/config",
        ctx.discovery_prefix,
        node_id(ctx),
        area.id
    )
}

pub fn area_discovery_payload(area: &Area, ctx: &Ctx) -> String {
    let prefix = ctx.topic_prefix;
    let name = if area.name.is_empty() {
        format!("Area {}", area.id)
    } else {
        area.name.clone()
    };

    let options = area.select_options();

    let mut payload = json!({
        "name": name,
        "unique_id": format!("spc_{}_area_{}", ctx.serial, area.id),
        "state_topic": format!("{prefix}/area/{}/state", area.id),
        "command_topic": format!("{prefix}/area/{}/set", area.id),
        "options": options,
        "icon": "mdi:shield-home",
        "device": device_info(ctx),
    });
    merge(&mut payload, &availability(ctx));

    payload.to_string()
}

// --- System sensors (binary_sensor with problem class) ---

pub fn system_sensor_discovery_topic(sensor_id: &str, ctx: &Ctx) -> String {
    format!(
        "{}/binary_sensor/{}/{sensor_id}/config",
        ctx.discovery_prefix,
        node_id(ctx),
    )
}

pub fn system_sensor_discovery_payload(
    sensor_id: &str,
    name: &str,
    state_topic: &str,
    ctx: &Ctx,
) -> String {
    let mut payload = json!({
        "name": name,
        "unique_id": format!("spc_{}_{sensor_id}", ctx.serial),
        "state_topic": state_topic,
        "payload_on": "ON",
        "payload_off": "OFF",
        "device_class": "problem",
        "entity_category": "diagnostic",
        "device": device_info(ctx),
    });
    merge(&mut payload, &availability(ctx));

    payload.to_string()
}

// --- Event log sensor ---

pub fn event_sensor_discovery_topic(ctx: &Ctx) -> String {
    format!(
        "{}/sensor/{}/last_event/config",
        ctx.discovery_prefix,
        node_id(ctx),
    )
}

pub fn event_sensor_discovery_payload(ctx: &Ctx) -> String {
    let prefix = ctx.topic_prefix;
    let mut payload = json!({
        "name": "Last Event",
        "unique_id": format!("spc_{}_last_event", ctx.serial),
        "state_topic": format!("{prefix}/event"),
        "value_template": "{{ value_json.text[:255] }}",
        "json_attributes_topic": format!("{prefix}/event"),
        "icon": "mdi:shield-alert",
        "device": device_info(ctx),
    });
    merge(&mut payload, &availability(ctx));

    payload.to_string()
}

// --- Zone action buttons (inhibit/isolate) ---

pub fn zone_button_discovery_topic(zone: &Zone, action: &str, ctx: &Ctx) -> String {
    format!(
        "{}/button/{}/zone_{}_{action}/config",
        ctx.discovery_prefix,
        node_id(ctx),
        zone.id,
    )
}

pub fn zone_button_discovery_payload(zone: &Zone, action: &str, ctx: &Ctx) -> String {
    let prefix = ctx.topic_prefix;
    let name = if zone.name.is_empty() {
        format!("Zone {} {}", zone.id, capitalize(action))
    } else {
        format!("{} {}", zone.name, capitalize(action))
    };

    let mut payload = json!({
        "name": name,
        "unique_id": format!("spc_{}_zone_{}_{action}", ctx.serial, zone.id),
        "command_topic": format!("{prefix}/zone/{}/action", zone.id),
        "payload_press": action,
        "entity_category": "config",
        "device": device_info(ctx),
    });
    merge(&mut payload, &availability(ctx));

    payload.to_string()
}

// --- Alert action buttons (inhibit/isolate) ---

pub fn alert_button_discovery_topic(sensor_id: &str, action: &str, ctx: &Ctx) -> String {
    format!(
        "{}/button/{}/{sensor_id}_{action}/config",
        ctx.discovery_prefix,
        node_id(ctx),
    )
}

pub fn alert_button_discovery_payload(
    sensor_id: &str,
    alert_name: &str,
    action: &str,
    ctx: &Ctx,
) -> String {
    let prefix = ctx.topic_prefix;
    let mut payload = json!({
        "name": format!("{alert_name} {}", capitalize(action)),
        "unique_id": format!("spc_{}_{sensor_id}_{action}", ctx.serial),
        "command_topic": format!("{prefix}/alert/{sensor_id}/action"),
        "payload_press": action,
        "entity_category": "config",
        "device": device_info(ctx),
    });
    merge(&mut payload, &availability(ctx));

    payload.to_string()
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().to_string() + c.as_str(),
    }
}
