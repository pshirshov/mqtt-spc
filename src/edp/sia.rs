//! SIA event payloads pushed by the panel (major 2):
//! `E2[#<panel_id>|<HHMMSSDDMMYYYY>|<code>|<address>|<description>|<extra>|<verification_id>]`

const ENVELOPE_START: &str = "E2[#";
const TIMESTAMP_LEN: usize = 14;
/// Firmware uses Latin-1 `¦` (0xA6) as a separator inside descriptions.
const LATIN1_BROKEN_BAR: u8 = 0xA6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiaEvent {
    pub panel_id: u32,
    /// Raw `HHMMSSDDMMYYYY`, panel-local wall time without a zone.
    pub timestamp: String,
    pub code: String,
    pub address: String,
    pub description: String,
}

impl SiaEvent {
    pub fn parse(payload: &[u8]) -> Result<Self, String> {
        let bytes: Vec<u8> = payload
            .iter()
            .flat_map(|&b| {
                if b == LATIN1_BROKEN_BAR {
                    "¦".as_bytes().to_vec()
                } else {
                    vec![b]
                }
            })
            .collect();
        let text = String::from_utf8_lossy(&bytes);
        let err = || format!("not a SIA event payload: {text:?}");

        let inner = text
            .trim()
            .strip_prefix(ENVELOPE_START)
            .and_then(|s| s.strip_suffix(']'))
            .ok_or_else(err)?;
        let (panel_id, rest) = inner.split_once('|').ok_or_else(err)?;
        let panel_id: u32 = panel_id.parse().map_err(|_| err())?;
        let (timestamp, rest) = rest.split_once('|').ok_or_else(err)?;
        if timestamp.len() != TIMESTAMP_LEN || !timestamp.bytes().all(|b| b.is_ascii_digit()) {
            return Err(err());
        }

        // Descriptions may contain '|': take code/address from the left and
        // the two trailing fixed fields from the right.
        let mut left = rest.splitn(3, '|');
        let (Some(code), Some(address), Some(tail)) = (left.next(), left.next(), left.next())
        else {
            return Err(err());
        };
        let mut right = tail.rsplitn(3, '|');
        let (Some(_verification_id), Some(_extra), Some(description)) =
            (right.next(), right.next(), right.next())
        else {
            return Err(err());
        };

        Ok(Self {
            panel_id,
            timestamp: timestamp.to_owned(),
            code: code.to_ascii_uppercase(),
            address: address.trim().to_owned(),
            description: description.to_owned(),
        })
    }

    /// Numeric address (zone or area, depending on the code).
    pub fn numeric_address(&self) -> Option<u32> {
        self.address.parse().ok()
    }

    /// `YYYY-MM-DD HH:MM:SS` in panel-local time.
    pub fn panel_time(&self) -> String {
        let t = &self.timestamp;
        format!(
            "{}-{}-{} {}:{}:{}",
            &t[10..14],
            &t[8..10],
            &t[6..8],
            &t[0..2],
            &t[2..4],
            &t[4..6]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_captured_event() {
        let ev = SiaEvent::parse(b"E2[#1000|09153403062026|NR|0|IP Link Restore||0]").unwrap();
        assert_eq!(ev.panel_id, 1000);
        assert_eq!(ev.code, "NR");
        assert_eq!(ev.numeric_address(), Some(0));
        assert_eq!(ev.description, "IP Link Restore");
        assert_eq!(ev.panel_time(), "2026-06-03 09:15:34");
    }

    #[test]
    fn description_with_pipes_and_broken_bar() {
        let ev =
            SiaEvent::parse(b"E2[#1000|08521203062026|ZO|1|Back|Door\xa6ZONE|xtra|7]\r\n").unwrap();
        assert_eq!(ev.code, "ZO");
        assert_eq!(ev.address, "1");
        assert_eq!(ev.description, "Back|Door¦ZONE");
    }

    #[test]
    fn rejects_garbage() {
        assert!(SiaEvent::parse(b"this is not an event").is_err());
        assert!(SiaEvent::parse(b"E2[#1000|0852|ZO|1|x||0]").is_err());
        assert!(SiaEvent::parse(b"E2[#1000|08521203062026|ZO]").is_err());
    }
}
