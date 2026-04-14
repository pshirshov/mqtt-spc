use scraper::{Html, Selector};
use tracing::debug;

use crate::model::{AreaAction, ZoneInput, ZoneState};

#[derive(Debug, Clone)]
pub struct ParsedArea {
    pub id: u32,
    pub name: String,
    pub state: String,
    pub actions: Vec<AreaAction>,
}

#[derive(Debug, Clone)]
pub struct ParsedZone {
    pub id: u32,
    pub name: String,
    pub area_id: u32,
    pub zone_type: String,
    pub input: ZoneInput,
    pub status: String,
}

impl ParsedZone {
    pub fn zone_state(&self) -> ZoneState {
        // Status column takes priority (alarm conditions), then input
        match self.status.as_str() {
            "Alarm" => ZoneState::Alarm,
            "Tamper" => ZoneState::Tamper,
            "Trouble" => ZoneState::Trouble,
            "Inhibited" => ZoneState::Inhibited,
            "Isolated" => ZoneState::Isolated,
            _ => match self.input {
                ZoneInput::Open => ZoneState::Open,
                ZoneInput::Closed => ZoneState::Closed,
                ZoneInput::Unknown => ZoneState::Closed,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParsedAlert {
    pub name: String,
    pub ok: bool,
    /// Button index from inhibitN/isolateN form names.
    pub button_index: u32,
}

/// Parse area states and action buttons from `page=system_summary`.
///
/// Each area row contains:
///   - "Area N: Name" in a subhead td
///   - State text (e.g. "Unset", "Fullset") in bold blue
///   - Submit buttons with VALUE=label and NAME=form_name
pub fn parse_areas(html: &str) -> Vec<ParsedArea> {
    let doc = Html::parse_document(html);
    let table_sel = Selector::parse("table#maintable").unwrap();
    let tr_sel = Selector::parse("tr").unwrap();
    let td_sel = Selector::parse("td").unwrap();
    let input_sel = Selector::parse("input[type='submit']").unwrap();

    let mut areas = Vec::new();

    let Some(maintable) = doc.select(&table_sel).next() else {
        return areas;
    };

    for row in maintable.select(&tr_sel) {
        let cells: Vec<scraper::ElementRef> = row.select(&td_sel).collect();
        if cells.len() < 3 {
            continue;
        }

        // Look for "Area N: Name" or "All Areas" in cells
        let mut area_id = None;
        let mut area_name = String::new();
        let mut is_all_areas = false;
        for cell in &cells {
            let text = cell.text().collect::<String>();
            let text = text.trim();
            if text == "All Areas" {
                area_id = Some(0u32);
                area_name = "All Areas".to_string();
                is_all_areas = true;
            } else if let Some(rest) = text.strip_prefix("Area ") {
                if let Some((id_str, name)) = rest.split_once(':') {
                    if let Ok(id) = id_str.trim().parse::<u32>() {
                        area_id = Some(id);
                        area_name = name.trim().to_string();
                    }
                }
            }
        }

        let Some(id) = area_id else { continue };

        // Find state text — look for bold blue text in cells
        let mut state = String::new();
        for cell in &cells {
            let style = cell.value().attr("style").unwrap_or("");
            if style.contains("color:blue") && style.contains("font-weight:bold") {
                let text = cell.text().collect::<String>().trim().to_string();
                if !text.is_empty() {
                    state = text;
                    break;
                }
            }
        }

        // Extract submit buttons from the row
        let mut actions = Vec::new();
        for input in row.select(&input_sel) {
            let name = input.value().attr("name").unwrap_or("");
            let value = input.value().attr("value").unwrap_or("").trim();
            if name.is_empty() || value.is_empty() {
                continue;
            }
            let matches = if is_all_areas {
                name.contains("all_areas")
            } else {
                (name.contains("set") || name.contains("unset"))
                    && name.contains(&format!("area{id}"))
            };
            if matches {
                actions.push(AreaAction {
                    label: value.to_string(),
                    form_name: name.to_string(),
                });
            }
        }

        if state.is_empty() {
            state = "Unset".into();
        }

        debug!(
            "Parsed area {id}: {area_name} = {state}, actions: {:?}",
            actions.iter().map(|a| &a.label).collect::<Vec<_>>()
        );

        areas.push(ParsedArea {
            id,
            name: area_name,
            state,
            actions,
        });
    }

    areas
}

/// Parse zone table from `page=status_zones`.
///
/// DOM columns: Zone | Area | Zone Type | Status | Log | Action
/// Input state (Open/Closed) is in HTML comments within each TR row.
pub fn parse_zones(html: &str) -> Vec<ParsedZone> {
    let doc = Html::parse_document(html);
    let table_sel = Selector::parse("table.gridtable").unwrap();
    let tr_sel = Selector::parse("tr").unwrap();
    let td_sel = Selector::parse("td").unwrap();

    // Extract Input states from HTML comments.
    // Pattern: <!-- ...<font color="...">Open|Closed</font>... -->
    let input_states = extract_commented_inputs(html);

    let mut zones = Vec::new();

    let Some(table) = doc.select(&table_sel).next() else {
        return zones;
    };

    let mut zone_idx = 0usize;
    for row in table.select(&tr_sel) {
        let cells: Vec<String> = row
            .select(&td_sel)
            .map(|td| td.text().collect::<String>().trim().to_string())
            .collect();

        // Need at least 4 columns: Zone, Area, Zone Type, Status
        if cells.len() < 4 {
            continue;
        }

        // Zone cell: "1 Front door" — id + name
        let (zone_id, zone_name) = match parse_id_name(&cells[0]) {
            Some(v) => v,
            None => continue,
        };

        // Area cell: "1 Ground floor"
        let (area_id, _) = parse_id_name(&cells[1]).unwrap_or((0, String::new()));

        let zone_type = cells[2].clone();
        let status = cells[3].clone();

        let input = input_states
            .get(zone_idx)
            .copied()
            .unwrap_or(ZoneInput::Unknown);
        zone_idx += 1;

        debug!("Parsed zone {zone_id}: {zone_name} input={input} status={status}");

        zones.push(ParsedZone {
            id: zone_id,
            name: zone_name,
            area_id,
            zone_type,
            input,
            status,
        });
    }

    zones
}

/// Extract Open/Closed values from HTML comments in zone table rows.
///
/// The panel comments out EOL Quality + Input columns:
///   `<!--  <TD>...</TD><TD><font color="green">Closed</font></TD>  -->`
fn extract_commented_inputs(html: &str) -> Vec<ZoneInput> {
    let mut inputs = Vec::new();
    let mut search_from = 0;

    while let Some(start) = html[search_from..].find("<!--") {
        let start = search_from + start;
        let Some(end) = html[start..].find("-->") else {
            break;
        };
        let end = start + end + 3;
        let comment = &html[start..end];
        search_from = end;

        // Look for <font ...>Open|Closed</font> inside the comment
        if let Some(input) = extract_font_text(comment) {
            match input {
                "Open" => inputs.push(ZoneInput::Open),
                "Closed" => inputs.push(ZoneInput::Closed),
                _ => {}
            }
        }
    }

    inputs
}

fn extract_font_text(comment: &str) -> Option<&str> {
    let marker = "<font";
    let pos = comment.find(marker)?;
    let after_tag = &comment[pos..];
    let gt = after_tag.find('>')?;
    let text_start = &after_tag[gt + 1..];
    let end = text_start.find('<')?;
    let text = text_start[..end].trim();
    if text.is_empty() {
        return None;
    }
    Some(text)
}

/// Parse system alerts from `page=status_sysalert`.
///
/// Table structure: `<TABLE CLASS="gridtable">` with columns:
///   Alert | Input | Status | Action
/// Both Input and Status show "OK" (green) or fault text.
pub fn parse_alerts(html: &str) -> Vec<ParsedAlert> {
    let doc = Html::parse_document(html);
    let table_sel = Selector::parse("table.gridtable").unwrap();
    let tr_sel = Selector::parse("tr").unwrap();
    let td_sel = Selector::parse("td").unwrap();
    let input_sel = Selector::parse("input[type='submit']").unwrap();

    let mut alerts = Vec::new();

    let Some(table) = doc.select(&table_sel).next() else {
        return alerts;
    };

    for row in table.select(&tr_sel) {
        let cells: Vec<String> = row
            .select(&td_sel)
            .map(|td| td.text().collect::<String>().trim().to_string())
            .collect();

        // Columns: Alert(0), Input(1), Status(2), Action(3)
        if cells.len() >= 3 && !cells[0].is_empty() {
            let input_ok = cells[1].eq_ignore_ascii_case("OK");
            let status_ok = cells[2].eq_ignore_ascii_case("OK");
            let ok = input_ok && status_ok;

            // Extract button index from inhibitN/isolateN button names
            let button_index = row
                .select(&input_sel)
                .find_map(|input| {
                    let name = input.value().attr("name").unwrap_or("");
                    name.strip_prefix("inhibit")
                        .or_else(|| name.strip_prefix("isolate"))
                        .and_then(|n| n.parse::<u32>().ok())
                })
                .unwrap_or(0);

            debug!("Parsed alert: {} input={} status={} btn_idx={button_index}", cells[0], cells[1], cells[2]);
            alerts.push(ParsedAlert {
                name: cells[0].clone(),
                ok,
                button_index,
            });
        }
    }

    alerts
}

/// Parse panel model name and serial from any panel HTML page.
///
/// The header contains: `SPC4300&nbsp;&nbsp;|&nbsp;&nbsp;Ver 3.9.0&nbsp;&nbsp;|&nbsp;&nbsp;R.32412&nbsp;&nbsp;|&nbsp;&nbsp;S/N: 643069802`
#[derive(Debug, Clone)]
pub struct PanelInfo {
    pub name: String,
    pub serial: String,
}

pub fn parse_panel_info(html: &str) -> Option<PanelInfo> {
    // Look for "S/N: <digits>" in the raw text
    let sn_marker = "S/N:";
    let sn_pos = html.find(sn_marker)?;
    let after_sn = &html[sn_pos + sn_marker.len()..];
    // Skip whitespace and &nbsp;
    let after_sn = after_sn.trim_start().trim_start_matches("&nbsp;");
    let serial_end = after_sn.find(|c: char| !c.is_ascii_digit()).unwrap_or(after_sn.len());
    let serial = after_sn[..serial_end].to_string();

    // Look for model name: "SPC" followed by digits (e.g. "SPC4300")
    // It appears in <title> and in the header text.
    let name = html
        .find("SPC")
        .and_then(|pos| {
            let rest = &html[pos..];
            // Take "SPC" + following digits
            let end = 3 + rest[3..].find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len() - 3);
            let name = rest[..end].trim().to_string();
            if name.len() >= 4 && name.len() <= 10 { Some(name) } else { None }
        })
        .unwrap_or_default();

    if serial.is_empty() {
        return None;
    }

    debug!("Parsed panel info: name={name} serial={serial}");
    Some(PanelInfo { name, serial })
}

/// Parse "N Name" into (id, name).
fn parse_id_name(text: &str) -> Option<(u32, String)> {
    let text = text.trim();
    let space = text.find(' ')?;
    let id: u32 = text[..space].parse().ok()?;
    let name = text[space + 1..].trim().to_string();
    Some((id, name))
}
