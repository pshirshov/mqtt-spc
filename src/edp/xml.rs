//! XML query channel (major 10).
//!
//! Each payload carries a 1-byte fragment marker (`0x01` first, `0x02`
//! continuation). A long reply is pulled chunk by chunk by re-sending the
//! same request with the continuation marker until `</COMMAND_REPLY>` shows up.

use std::collections::HashMap;

const FRAG_FIRST: u8 = 0x01;
const FRAG_CONT: u8 = 0x02;
const REPLY_CLOSE: &[u8] = b"</COMMAND_REPLY>";
/// Backstop for a reply that never closes; real status dumps are a few KB.
const MAX_REPLY_BYTES: usize = 1 << 20;

/// Attributes of one XML element as sent by the panel.
pub type Row = HashMap<String, String>;

/// A parsed `<COMMAND_REPLY>`: section tag -> child rows. A leaf section
/// (e.g. `<INFO .../>`) becomes a single row of its own attributes.
#[derive(Debug, Default)]
pub struct XmlReply {
    sections: HashMap<String, Vec<Row>>,
}

impl XmlReply {
    pub fn rows(&self, section: &str) -> &[Row] {
        self.sections.get(section).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// `command_id` is one of our fixed ASCII identifiers; no escaping is needed.
pub fn build_request(command_id: &str, continuation: bool) -> Vec<u8> {
    let marker = if continuation { FRAG_CONT } else { FRAG_FIRST };
    let mut payload = vec![marker];
    payload.extend_from_slice(format!("<COMMAND ID=\"{command_id}\" />").as_bytes());
    payload
}

#[derive(Default)]
pub struct ReplyAssembler {
    buf: Vec<u8>,
}

impl ReplyAssembler {
    /// Returns the assembled XML once the closing tag has been received.
    pub fn feed(&mut self, payload: &[u8]) -> Result<Option<Vec<u8>>, String> {
        if let Some(body) = payload.get(1..) {
            self.buf.extend_from_slice(body);
        }
        if self.buf.len() > MAX_REPLY_BYTES {
            return Err(format!(
                "XML reply exceeded {MAX_REPLY_BYTES} bytes without closing"
            ));
        }
        if self
            .buf
            .windows(REPLY_CLOSE.len())
            .any(|w| w == REPLY_CLOSE)
        {
            return Ok(Some(std::mem::take(&mut self.buf)));
        }
        Ok(None)
    }
}

pub fn parse_reply(xml: &[u8]) -> Result<XmlReply, String> {
    // Panel strings are Latin-1 in practice; map bytes 1:1 so a non-UTF-8
    // zone name cannot fail the whole snapshot.
    let text: String = match std::str::from_utf8(xml) {
        Ok(s) => s.to_owned(),
        Err(_) => xml.iter().map(|&b| char::from(b)).collect(),
    };
    // roxmltree rejects DTDs by default, so entity expansion cannot occur.
    let doc = roxmltree::Document::parse(text.trim_matches(char::from(0)))
        .map_err(|e| format!("malformed XML reply: {e}"))?;
    let root = doc.root_element();
    if root.tag_name().name() != "COMMAND_REPLY" {
        return Err(format!(
            "expected COMMAND_REPLY, got <{}>",
            root.tag_name().name()
        ));
    }
    let attrs = |node: roxmltree::Node| -> Row {
        node.attributes()
            .map(|a| (a.name().to_owned(), a.value().to_owned()))
            .collect()
    };
    let mut reply = XmlReply::default();
    for section in root.children().filter(roxmltree::Node::is_element) {
        let children: Vec<Row> = section
            .children()
            .filter(roxmltree::Node::is_element)
            .map(attrs)
            .collect();
        let rows = if children.is_empty() {
            vec![attrs(section)]
        } else {
            children
        };
        reply
            .sections
            .insert(section.tag_name().name().to_owned(), rows);
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_markers() {
        assert_eq!(build_request("info", false), b"\x01<COMMAND ID=\"info\" />");
        assert_eq!(build_request("zone_status", true)[0], FRAG_CONT);
    }

    #[test]
    fn assembles_fragments() {
        let mut asm = ReplyAssembler::default();
        assert!(
            asm.feed(b"\x01<COMMAND_REPLY><ZONE_STATUS><ZONE ID=\"1\" />")
                .unwrap()
                .is_none()
        );
        let xml = asm
            .feed(b"\x02</ZONE_STATUS></COMMAND_REPLY>")
            .unwrap()
            .unwrap();
        let reply = parse_reply(&xml).unwrap();
        assert_eq!(reply.rows("ZONE_STATUS")[0]["ID"], "1");
    }

    #[test]
    fn leaf_section_is_single_row() {
        let reply =
            parse_reply(b"<COMMAND_REPLY><INFO TYPE=\"SPC4300\" SN=\"123\" /></COMMAND_REPLY>")
                .unwrap();
        assert_eq!(reply.rows("INFO")[0]["SN"], "123");
        assert!(reply.rows("AREA_STATUS").is_empty());
    }

    #[test]
    fn rejects_dtd_and_wrong_root() {
        assert!(parse_reply(b"<!DOCTYPE x [<!ENTITY a \"b\">]><COMMAND_REPLY/>").is_err());
        assert!(parse_reply(b"<NOPE />").is_err());
    }

    #[test]
    fn latin1_names_survive() {
        let reply = parse_reply(
            b"<COMMAND_REPLY><ZONE_STATUS><ZONE ID=\"1\" ZONE_NAME=\"K\xfcche\" /></ZONE_STATUS></COMMAND_REPLY>",
        )
        .unwrap();
        assert_eq!(reply.rows("ZONE_STATUS")[0]["ZONE_NAME"], "Küche");
    }
}
