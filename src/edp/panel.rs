//! Typed panel snapshot built from XML queries and updated by SIA events.

use std::collections::BTreeMap;
use std::fmt;

use tracing::warn;

use super::sia::SiaEvent;
use super::xml::{Row, XmlReply};

pub const QUERY_INFO: &str = "info";
pub const QUERY_AREAS: &str = "area_status";
pub const QUERY_ZONES: &str = "zone_status";

/// Arm/disarm events do not reliably encode the mode, nor even the area
/// (some firmware puts the user ID in the address): re-read `area_status`.
const AREA_REFRESH_CODES: [&str; 6] = ["BV", "CG", "CL", "NL", "OG", "OP"];
/// Alarm families that identify a zone and imply it is open.
const ZONE_ALARM_CODES: [&str; 5] = ["BA", "FA", "PA", "HA", "TA"];
const ZONE_OPEN_CODE: &str = "ZO";
const ZONE_CLOSE_CODE: &str = "ZC";
/// Verified burglary alarm; addresses an area.
const AREA_ALARM_CODE: &str = "BV";

#[derive(Debug, Clone, Default)]
pub struct PanelInfo {
    pub model: String,
    pub serial: String,
    pub firmware: String,
}

impl PanelInfo {
    pub fn from_reply(reply: &XmlReply) -> Result<Self, String> {
        let row = reply
            .rows("INFO")
            .first()
            .ok_or("INFO reply has no INFO section")?;
        let get = |k: &str| row.get(k).cloned().unwrap_or_default();
        let info = Self {
            model: get("TYPE"),
            serial: get("SN"),
            firmware: get("VERSION"),
        };
        if info.serial.is_empty() {
            return Err(format!("INFO reply has no serial number: {row:?}"));
        }
        Ok(info)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArmMode {
    Unset,
    PartSetA,
    PartSetB,
    FullSet,
}

impl ArmMode {
    fn from_token(token: &str) -> Option<Self> {
        match token {
            "0" => Some(Self::Unset),
            "1" => Some(Self::PartSetA),
            "2" => Some(Self::PartSetB),
            "3" => Some(Self::FullSet),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Area {
    pub id: u8,
    pub name: String,
    /// `None` for a MODE token this bridge does not recognise.
    pub mode: Option<ArmMode>,
    /// Event-derived; AREA_STATUS has no alarm field. Cleared once the
    /// panel reports the area unset.
    pub triggered: bool,
}

/// Physical input as reported by ZONE_STATUS `INPUT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneInput {
    Closed,
    Open,
    Short,
    Disconnected,
    PirMasked,
    DcSubstitution,
    SensorMissing,
    Offline,
    Unknown,
}

impl ZoneInput {
    fn from_token(token: &str) -> Self {
        match token {
            "0" => Self::Closed,
            "1" => Self::Open,
            "2" => Self::Short,
            "3" => Self::Disconnected,
            "4" => Self::PirMasked,
            "5" => Self::DcSubstitution,
            "6" => Self::SensorMissing,
            "7" => Self::Offline,
            _ => Self::Unknown,
        }
    }

    /// `None` for fault/supervision states: they say nothing about open/closed.
    pub fn is_open(self) -> Option<bool> {
        match self {
            Self::Open => Some(true),
            Self::Closed => Some(false),
            _ => None,
        }
    }
}

impl fmt::Display for ZoneInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Closed => "closed",
            Self::Open => "open",
            Self::Short => "short",
            Self::Disconnected => "disconnected",
            Self::PirMasked => "pir_masked",
            Self::DcSubstitution => "dc_substitution",
            Self::SensorMissing => "sensor_missing",
            Self::Offline => "offline",
            Self::Unknown => "unknown",
        };
        f.write_str(s)
    }
}

/// ZONE_STATUS `TYPE` (SPC Web Gateway numbering).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneType {
    Alarm,
    EntryExit,
    ExitTerminator,
    Fire,
    FireExit,
    Line,
    Panic,
    HoldUp,
    Tamper,
    Technical,
    Medical,
    KeyArm,
    Unused,
    Shunt,
    XShunt,
    Fault,
    LockSupervision,
    Seismic,
    AllOkay,
    HoldUpFault,
    WarningFault,
    SettingAuthorisation,
    LockElement,
    Glassbreak,
    Water,
    Heat,
    FridgeFreezer,
    Gas,
    Sprinkler,
    Co,
    EntryExit2,
    Other(u32),
}

impl ZoneType {
    fn from_token(token: &str) -> Option<Self> {
        let n: u32 = token.parse().ok()?;
        const TABLE: [ZoneType; 31] = [
            ZoneType::Alarm,
            ZoneType::EntryExit,
            ZoneType::ExitTerminator,
            ZoneType::Fire,
            ZoneType::FireExit,
            ZoneType::Line,
            ZoneType::Panic,
            ZoneType::HoldUp,
            ZoneType::Tamper,
            ZoneType::Technical,
            ZoneType::Medical,
            ZoneType::KeyArm,
            ZoneType::Unused,
            ZoneType::Shunt,
            ZoneType::XShunt,
            ZoneType::Fault,
            ZoneType::LockSupervision,
            ZoneType::Seismic,
            ZoneType::AllOkay,
            ZoneType::HoldUpFault,
            ZoneType::WarningFault,
            ZoneType::SettingAuthorisation,
            ZoneType::LockElement,
            ZoneType::Glassbreak,
            ZoneType::Water,
            ZoneType::Heat,
            ZoneType::FridgeFreezer,
            ZoneType::Gas,
            ZoneType::Sprinkler,
            ZoneType::Co,
            ZoneType::EntryExit2,
        ];
        Some(TABLE.get(n as usize).copied().unwrap_or(ZoneType::Other(n)))
    }

    /// Home Assistant binary_sensor device class.
    pub fn device_class(self) -> &'static str {
        match self {
            Self::EntryExit | Self::EntryExit2 | Self::ExitTerminator => "door",
            Self::Alarm => "motion",
            Self::Fire | Self::FireExit => "smoke",
            Self::Tamper => "tamper",
            Self::Water => "moisture",
            Self::Heat => "heat",
            Self::Gas => "gas",
            Self::Co => "carbon_monoxide",
            Self::Glassbreak | Self::Seismic => "vibration",
            Self::Panic | Self::HoldUp | Self::Medical => "safety",
            _ => "opening",
        }
    }
}

impl fmt::Display for ZoneType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Other(n) => write!(f, "type_{n}"),
            other => write!(f, "{other:?}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    pub id: u32,
    pub name: String,
    pub area_id: u32,
    pub zone_type: Option<ZoneType>,
    pub input: ZoneInput,
}

/// Which snapshot entries an event changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EventEffect {
    pub zones: Vec<u32>,
    pub areas: Vec<u8>,
    /// The event cannot be applied locally; re-read `area_status`.
    pub refresh_areas: bool,
}

#[derive(Debug, Default)]
pub struct Snapshot {
    pub areas: BTreeMap<u8, Area>,
    pub zones: BTreeMap<u32, Zone>,
}

fn parse_id<T: std::str::FromStr>(row: &Row, kind: &str) -> Option<T> {
    let id = row.get("ID").and_then(|v| v.parse().ok());
    if id.is_none() {
        warn!("Skipping {kind} row with missing/invalid ID: {row:?}");
    }
    id
}

impl Snapshot {
    pub fn apply_areas(&mut self, reply: &XmlReply) {
        let mut areas = BTreeMap::new();
        for row in reply.rows("AREA_STATUS") {
            let Some(id) = parse_id::<u8>(row, "area") else {
                continue;
            };
            let token = row.get("MODE").map(String::as_str).unwrap_or_default();
            let mode = ArmMode::from_token(token);
            if mode.is_none() {
                warn!("Area {id} has unrecognised MODE {token:?}");
            }
            let was_triggered = self.areas.get(&id).is_some_and(|a| a.triggered);
            areas.insert(
                id,
                Area {
                    id,
                    name: row.get("NAME").cloned().unwrap_or_default(),
                    mode,
                    triggered: was_triggered && mode != Some(ArmMode::Unset),
                },
            );
        }
        self.areas = areas;
    }

    pub fn apply_zones(&mut self, reply: &XmlReply) {
        self.zones = reply
            .rows("ZONE_STATUS")
            .iter()
            .filter_map(|row| {
                let id = parse_id::<u32>(row, "zone")?;
                let get = |k: &str| row.get(k).map(String::as_str).unwrap_or_default();
                Some((
                    id,
                    Zone {
                        id,
                        name: get("ZONE_NAME").to_owned(),
                        area_id: get("AREA").parse().unwrap_or(0),
                        zone_type: ZoneType::from_token(get("TYPE")),
                        input: ZoneInput::from_token(get("INPUT")),
                    },
                ))
            })
            .collect();
    }

    /// Apply an event locally where it is unambiguous.
    pub fn apply_event(&mut self, ev: &SiaEvent) -> EventEffect {
        let code = ev.code.as_str();
        if AREA_REFRESH_CODES.contains(&code) {
            let mut effect = EventEffect {
                refresh_areas: true,
                ..Default::default()
            };
            if code == AREA_ALARM_CODE
                && let Some(area) = ev
                    .numeric_address()
                    .and_then(|a| u8::try_from(a).ok())
                    .and_then(|a| self.areas.get_mut(&a))
            {
                area.triggered = true;
                effect.areas.push(area.id);
            }
            return effect;
        }

        let alarm = ZONE_ALARM_CODES.contains(&code);
        let open = match code {
            ZONE_OPEN_CODE => true,
            ZONE_CLOSE_CODE => false,
            _ if alarm => true,
            _ => return EventEffect::default(),
        };
        let Some(zone) = ev.numeric_address().and_then(|a| self.zones.get_mut(&a)) else {
            return EventEffect::default();
        };
        zone.input = if open {
            ZoneInput::Open
        } else {
            ZoneInput::Closed
        };
        let mut effect = EventEffect {
            zones: vec![zone.id],
            ..Default::default()
        };
        if alarm
            && let Some(area) = u8::try_from(zone.area_id)
                .ok()
                .and_then(|a| self.areas.get_mut(&a))
        {
            area.triggered = true;
            effect.areas.push(area.id);
        }
        effect
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edp::xml::parse_reply;

    fn snapshot() -> Snapshot {
        let mut s = Snapshot::default();
        s.apply_areas(
            &parse_reply(
                br#"<COMMAND_REPLY><AREA_STATUS><AREA ID="1" NAME="House" MODE="3" /></AREA_STATUS></COMMAND_REPLY>"#,
            )
            .unwrap(),
        );
        s.apply_zones(
            &parse_reply(
                br#"<COMMAND_REPLY><ZONE_STATUS>
                    <ZONE ID="1" ZONE_NAME="Front door" AREA="1" TYPE="1" INPUT="0" />
                    <ZONE ID="2" ZONE_NAME="Hall PIR" AREA="1" TYPE="0" INPUT="7" />
                    <ZONE ID="x" />
                </ZONE_STATUS></COMMAND_REPLY>"#,
            )
            .unwrap(),
        );
        s
    }

    fn event(code: &str, addr: &str) -> SiaEvent {
        SiaEvent::parse(format!("E2[#1000|08521203062026|{code}|{addr}|x||0]").as_bytes()).unwrap()
    }

    #[test]
    fn parses_rows() {
        let s = snapshot();
        assert_eq!(s.areas[&1].mode, Some(ArmMode::FullSet));
        assert_eq!(s.zones.len(), 2);
        assert_eq!(s.zones[&1].zone_type, Some(ZoneType::EntryExit));
        assert_eq!(s.zones[&1].input.is_open(), Some(false));
        assert_eq!(s.zones[&2].input.is_open(), None);
    }

    #[test]
    fn zone_events() {
        let mut s = snapshot();
        assert_eq!(s.apply_event(&event("ZO", "1")).zones, vec![1]);
        assert_eq!(s.zones[&1].input, ZoneInput::Open);
        s.apply_event(&event("ZC", "1"));
        assert_eq!(s.zones[&1].input, ZoneInput::Closed);
        assert_eq!(s.apply_event(&event("ZO", "99")), EventEffect::default());
    }

    #[test]
    fn alarm_triggers_area_until_unset() {
        let mut s = snapshot();
        let effect = s.apply_event(&event("BA", "2"));
        assert_eq!((effect.zones, effect.areas), (vec![2], vec![1]));
        assert!(s.areas[&1].triggered);

        let still_set = parse_reply(
            br#"<COMMAND_REPLY><AREA_STATUS><AREA ID="1" NAME="House" MODE="3" /></AREA_STATUS></COMMAND_REPLY>"#,
        )
        .unwrap();
        s.apply_areas(&still_set);
        assert!(s.areas[&1].triggered);

        let unset = parse_reply(
            br#"<COMMAND_REPLY><AREA_STATUS><AREA ID="1" NAME="House" MODE="0" /></AREA_STATUS></COMMAND_REPLY>"#,
        )
        .unwrap();
        s.apply_areas(&unset);
        assert!(!s.areas[&1].triggered);
    }

    #[test]
    fn arm_events_request_area_refresh() {
        let mut s = snapshot();
        assert!(s.apply_event(&event("CL", "3")).refresh_areas);
        let bv = s.apply_event(&event("BV", "1"));
        assert!(bv.refresh_areas && s.areas[&1].triggered);
    }
}
