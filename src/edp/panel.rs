//! Typed panel snapshot built from XML queries and updated by SIA events.

use std::collections::BTreeMap;
use std::fmt;

use tracing::warn;

use super::sia::SiaEvent;
use super::xml::{Row, XmlReply};

pub const QUERY_INFO: &str = "info";
pub const QUERY_AREAS: &str = "area_status";
pub const QUERY_ZONES: &str = "zone_status";
pub const QUERY_STATUS: &str = "status";

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

/// ZONE_STATUS `STATUS`. Values 1 and 2 verified by inhibiting/isolating a
/// zone on a live panel; others are passed through as their raw number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneStatus {
    Normal,
    Inhibited,
    Isolated,
    Other(u32),
}

impl ZoneStatus {
    fn from_token(token: &str) -> Self {
        match token.parse::<u32>() {
            Ok(0) => Self::Normal,
            Ok(1) => Self::Inhibited,
            Ok(2) => Self::Isolated,
            Ok(n) => Self::Other(n),
            Err(_) => {
                warn!("Zone has non-numeric STATUS {token:?}");
                Self::Other(u32::MAX)
            }
        }
    }
}

impl fmt::Display for ZoneStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Normal => f.write_str("normal"),
            Self::Inhibited => f.write_str("inhibited"),
            Self::Isolated => f.write_str("isolated"),
            Self::Other(n) => write!(f, "status_{n}"),
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
    pub status: ZoneStatus,
    pub inhibit_allowed: bool,
    pub isolate_allowed: bool,
}

/// A system alert and its bit in the `status` reply's SYSALERT masks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemAlertDef {
    pub bit: u32,
    /// Stable identifier; matches the web-UI-based bridge's sensor IDs.
    pub slug: &'static str,
    pub name: &'static str,
}

/// Measured on a live SPC4000 (firmware 3.9.0) by inhibiting each alert via
/// the web UI and reading SYSALERT.INHIBIT. User Duress, User RF FOB Panic
/// and Code Tamper cannot be inhibited, so their bits are unknown; the two
/// wireless relearning alerts set no bit.
pub const SYSTEM_ALERTS: [SystemAlertDef; 17] = [
    SystemAlertDef { bit: 0, slug: "mains_fault", name: "Mains Fault" },
    SystemAlertDef { bit: 1, slug: "battery_fault", name: "Battery Fault" },
    SystemAlertDef { bit: 2, slug: "aux_fuse_fault", name: "Aux. Fuse Fault" },
    SystemAlertDef { bit: 3, slug: "external_bell_fuse_fault", name: "External Bell Fuse Fault" },
    SystemAlertDef { bit: 4, slug: "internal_bell_fuse_fault", name: "Internal Bell Fuse Fault" },
    SystemAlertDef { bit: 5, slug: "bell_tamper", name: "Bell Tamper" },
    SystemAlertDef { bit: 6, slug: "cabinet_tamper", name: "Cabinet Tamper" },
    SystemAlertDef { bit: 7, slug: "aux_tamper_1", name: "Aux. Tamper 1" },
    SystemAlertDef { bit: 8, slug: "aux_tamper_2", name: "Aux. Tamper 2" },
    SystemAlertDef { bit: 9, slug: "antenna_tamper", name: "Antenna Tamper" },
    SystemAlertDef { bit: 10, slug: "rf_jamming", name: "RF Jamming" },
    SystemAlertDef { bit: 11, slug: "modem_1_fault", name: "Modem 1 Fault" },
    SystemAlertDef { bit: 15, slug: "x-bus_cable_fault", name: "X-BUS Cable Fault" },
    SystemAlertDef { bit: 16, slug: "fail_to_communicate", name: "Fail to Communicate" },
    SystemAlertDef { bit: 21, slug: "psu_fault", name: "PSU Fault" },
    SystemAlertDef { bit: 23, slug: "ethernet_link", name: "Ethernet Link" },
    SystemAlertDef { bit: 24, slug: "network_fault", name: "Network Fault" },
];

/// SYSALERT bitmasks from the `status` reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SystemAlerts {
    pub input: u32,
    pub alert: u32,
    pub inhibit: u32,
    pub isolate: u32,
}

impl SystemAlerts {
    pub fn from_reply(reply: &XmlReply) -> Result<Self, String> {
        let row = reply.rows("SYSALERT").first().ok_or("status reply has no SYSALERT")?;
        let mask = |k: &str| -> Result<u32, String> {
            let v = row.get(k).ok_or_else(|| format!("SYSALERT has no {k}: {row:?}"))?;
            u32::from_str_radix(v, 16).map_err(|e| format!("SYSALERT {k}={v:?}: {e}"))
        };
        Ok(Self {
            input: mask("INPUT")?,
            alert: mask("ALERT")?,
            inhibit: mask("INHIBIT")?,
            isolate: mask("ISOLATE")?,
        })
    }

    /// The alert is in fault either at its input or as a latched alert.
    pub fn is_active(&self, bit: u32) -> bool {
        (self.input | self.alert) >> bit & 1 == 1
    }

    pub fn is_inhibited(&self, bit: u32) -> bool {
        self.inhibit >> bit & 1 == 1
    }

    pub fn is_isolated(&self, bit: u32) -> bool {
        self.isolate >> bit & 1 == 1
    }

    /// Active bits with no entry in [`SYSTEM_ALERTS`].
    pub fn unmapped_active(&self) -> Vec<u32> {
        (0..u32::BITS)
            .filter(|&b| self.is_active(b) && !SYSTEM_ALERTS.iter().any(|d| d.bit == b))
            .collect()
    }
}

/// Which snapshot entries an event changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EventEffect {
    pub zones: Vec<u32>,
    pub areas: Vec<u8>,
    /// The event cannot be applied locally; re-read `area_status`.
    pub refresh_areas: bool,
    /// The event may concern a system alert; re-read `status`.
    pub refresh_alerts: bool,
}

#[derive(Debug, Default)]
pub struct Snapshot {
    pub areas: BTreeMap<u8, Area>,
    pub zones: BTreeMap<u32, Zone>,
    pub alerts: SystemAlerts,
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
                        status: ZoneStatus::from_token(get("STATUS")),
                        inhibit_allowed: get("INHIBIT_ALLOWED") == "1",
                        isolate_allowed: get("ISOLATE_ALLOWED") == "1",
                    },
                ))
            })
            .collect();
    }

    pub fn apply_status(&mut self, reply: &XmlReply) -> Result<(), String> {
        self.alerts = SystemAlerts::from_reply(reply)?;
        Ok(())
    }

    /// Apply an event locally where it is unambiguous.
    pub fn apply_event(&mut self, ev: &SiaEvent) -> EventEffect {
        let mut effect = self.apply_event_state(ev);
        effect.refresh_alerts = ![ZONE_OPEN_CODE, ZONE_CLOSE_CODE].contains(&ev.code.as_str());
        effect
    }

    fn apply_event_state(&mut self, ev: &SiaEvent) -> EventEffect {
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
                    <ZONE ID="1" ZONE_NAME="Front door" AREA="1" TYPE="1" INPUT="0" STATUS="0" INHIBIT_ALLOWED="1" ISOLATE_ALLOWED="1" />
                    <ZONE ID="2" ZONE_NAME="Hall PIR" AREA="1" TYPE="0" INPUT="7" STATUS="2" INHIBIT_ALLOWED="0" ISOLATE_ALLOWED="1" />
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
        assert_eq!(s.zones[&1].status, ZoneStatus::Normal);
        assert_eq!(s.zones[&2].status, ZoneStatus::Isolated);
        assert!(s.zones[&1].inhibit_allowed && !s.zones[&2].inhibit_allowed);
    }

    // Captured from the live panel, with the INHIBIT mask as observed while
    // Bell Tamper (bit 5) and Ethernet link (bit 23) were inhibited.
    const STATUS_REPLY: &[u8] = br#"<COMMAND_REPLY><SYSINFO TIME="09482630092026" ENGMODE="0" RF_TYPE="0" RF_VERSION="0" /><PSU BATT_VOLT="13.5V" AUX_VOLT="13.6V" AUX_CURR="100mA" AC_FREQ="50Hz" /><SYSALERT INPUT="00000000" ALERT="00000002" INHIBIT="00800020" ISOLATE="00000000" /><ETHERNETINFO FITTED="1" STATE="1" /></COMMAND_REPLY>"#;

    #[test]
    fn system_alert_masks() {
        let mut s = snapshot();
        s.apply_status(&parse_reply(STATUS_REPLY).unwrap()).unwrap();
        assert!(s.alerts.is_active(1), "battery fault via ALERT mask");
        assert!(!s.alerts.is_active(0));
        assert!(s.alerts.is_inhibited(5) && s.alerts.is_inhibited(23));
        assert!(s.alerts.unmapped_active().is_empty());
        let unknown = SystemAlerts { input: 1 << 30, ..Default::default() };
        assert_eq!(unknown.unmapped_active(), vec![30]);
        let bad = parse_reply(br#"<COMMAND_REPLY><SYSALERT INPUT="zz" /></COMMAND_REPLY>"#).unwrap();
        assert!(s.apply_status(&bad).is_err());
    }

    #[test]
    fn alert_refresh_on_non_zone_events() {
        let mut s = snapshot();
        assert!(s.apply_event(&event("NR", "0")).refresh_alerts);
        assert!(s.apply_event(&event("BA", "2")).refresh_alerts);
        assert!(!s.apply_event(&event("ZO", "1")).refresh_alerts);
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
