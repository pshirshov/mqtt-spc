use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneInput {
    Unknown,
    Open,
    Closed,
}

impl fmt::Display for ZoneInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown => f.write_str("unknown"),
            Self::Open => f.write_str("Open"),
            Self::Closed => f.write_str("Closed"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneState {
    Unknown,
    Open,
    Closed,
    Alarm,
    Tamper,
    Trouble,
    Inhibited,
    Isolated,
}

impl ZoneState {
    pub fn is_on(self) -> bool {
        matches!(self, Self::Open | Self::Alarm | Self::Tamper | Self::Trouble)
    }
}

impl fmt::Display for ZoneState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Unknown => "unknown",
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Alarm => "alarm",
            Self::Tamper => "tamper",
            Self::Trouble => "trouble",
            Self::Inhibited => "inhibited",
            Self::Isolated => "isolated",
        };
        f.write_str(s)
    }
}

/// An action button parsed from the panel's system_summary page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AreaAction {
    /// Display label from the button VALUE (e.g. "Ground Floor", "Fullset").
    pub label: String,
    /// HTML form field NAME (e.g. "partset_a_area1", "fullset_area1").
    pub form_name: String,
}

#[derive(Debug, Clone)]
pub struct Zone {
    pub id: u32,
    pub name: String,
    pub area_id: u32,
    pub zone_type: String,
    pub input: ZoneInput,
    pub status: String,
    pub state: ZoneState,
    pub device_class: String,
}

impl Zone {
    pub fn new(id: u32) -> Self {
        Self {
            id,
            name: String::new(),
            area_id: 0,
            zone_type: String::new(),
            input: ZoneInput::Unknown,
            status: String::new(),
            state: ZoneState::Unknown,
            device_class: String::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Area {
    pub id: u32,
    pub name: String,
    /// Current state as displayed by the panel (e.g. "Unset", "Fullset").
    pub state: String,
    /// Available actions parsed from submit buttons.
    pub actions: Vec<AreaAction>,
    /// All options ever seen (states + action labels), accumulated across polls.
    pub all_options: Vec<String>,
}

impl Area {
    pub fn new(id: u32) -> Self {
        Self {
            id,
            name: String::new(),
            state: "Unset".into(),
            actions: Vec::new(),
            all_options: Vec::new(),
        }
    }

    /// Accumulate new options from current state and actions.
    pub fn update_options(&mut self) {
        for label in std::iter::once(&self.state)
            .chain(self.actions.iter().map(|a| &a.label))
        {
            if !self.all_options.contains(label) {
                self.all_options.push(label.clone());
            }
        }
    }

    pub fn select_options(&self) -> &[String] {
        &self.all_options
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemAlert {
    pub name: String,
    pub ok: bool,
    /// Index used in form button names (e.g. inhibit0, isolate0).
    pub button_index: u32,
}

#[derive(Debug)]
pub struct PanelState {
    pub name: String,
    pub serial: String,
    pub zones: HashMap<u32, Zone>,
    pub areas: HashMap<u32, Area>,
    pub alerts: Vec<SystemAlert>,
}

impl PanelState {
    pub fn new() -> Self {
        Self {
            name: String::new(),
            serial: String::new(),
            zones: HashMap::new(),
            areas: HashMap::new(),
            alerts: Vec::new(),
        }
    }
}
