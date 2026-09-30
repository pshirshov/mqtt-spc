//! EDP receiver: the panel dials in to us over TCP.
//!
//! ```text
//! panel -> HELLO (1.2)         us -> HELLO_ACK (1.3, echo)
//! panel -> POLL  (1.0) ~10s    us -> POLL_ACK  (1.1, echo)
//! panel -> SIA event (2.0)     us -> EVENT_ACK (2.1, echo)
//! us    -> XML query (10.0)    panel -> reply (10.1, same sequence)
//! us    -> area cmd (4.0)      panel -> status (4.2, same sequence)
//! ```
//!
//! Commands and events share one sequence space. Commands are serialized,
//! and each waits for a short quiet period after the last received frame so
//! its sequence cannot collide with a queued event burst (which makes the
//! panel drop the connection).

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use super::sia::SiaEvent;
use super::wire::{DecodeError, EdpKey, FLAG_FROM_RECEIVER, Frame, FrameDecoder, major, minor};
use super::xml::{ReplyAssembler, XmlReply, build_request, parse_reply};

const COMMAND_QUIET_TIME: Duration = Duration::from_millis(100);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_XML_FRAGMENTS: usize = 64;
const READ_BUF_LEN: usize = 4096;
const WRITE_QUEUE_LEN: usize = 256;
/// SIA events buffered for the consumer before the oldest are dropped.
pub const LINK_EVENT_QUEUE_LEN: usize = 1024;
const REPLY_OK: u8 = 0xF0;

#[derive(Debug, Clone)]
pub struct ReceiverConfig {
    pub listen: SocketAddr,
    pub receiver_id: u32,
    pub key: Option<EdpKey>,
    pub idle_timeout: Duration,
}

/// Identifies one TCP connection, so events from a superseded connection
/// can be told apart from the current one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionId(u64);

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

#[derive(Debug)]
pub enum LinkEvent {
    /// The panel completed its first poll; commands may be sent.
    Ready(SessionHandle),
    Sia(SessionId, SiaEvent),
    Closed(SessionId),
}

#[derive(Debug)]
pub enum CommandError {
    Timeout(String),
    ConnectionLost,
    Protocol(String),
    Rejected { command: String, code: u8 },
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout(what) => write!(f, "timed out waiting for {what}"),
            Self::ConnectionLost => f.write_str("panel connection lost"),
            Self::Protocol(msg) => write!(f, "protocol error: {msg}"),
            Self::Rejected { command, code } => {
                write!(
                    f,
                    "panel rejected {command}: {} (code {code:#04x})",
                    reply_message(*code)
                )
            }
        }
    }
}

impl std::error::Error for CommandError {}

fn reply_message(code: u8) -> &'static str {
    match code {
        0xF2 => "invalid parameters",
        0xF3 => "panel is waiting for data",
        0xF4 => "panel is in full engineer mode",
        0xF5 => "command is not possible now",
        0xFB => "command is not permitted for this receiver",
        0xFC => "not implemented (also returned in engineer mode)",
        _ => "unknown reply code",
    }
}

/// Targeted binary commands (major 4; payload: opcode, target ID, 0).
/// All verified against a live SPC4000, firmware 3.9.0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    AreaFullSet = 0x01,
    AreaUnset = 0x02,
    ZoneInhibit = 0x03,
    ZoneDeinhibit = 0x04,
    ZoneIsolate = 0x09,
    ZoneDeisolate = 0x0A,
    AreaPartSetA = 0x0F,
    AreaPartSetB = 0x10,
}

type ReplyKey = (u8, u32);

struct Shared {
    receiver_id: u32,
    panel_id: u32,
    key: Option<EdpKey>,
    write_tx: mpsc::Sender<Vec<u8>>,
    link: StdMutex<LinkState>,
    /// Serializes whole command transactions, XML continuations included.
    command_lock: Mutex<()>,
}

struct LinkState {
    next_seq: u32,
    last_received: Instant,
    pending: HashMap<ReplyKey, oneshot::Sender<Vec<u8>>>,
    closed: bool,
}

impl Shared {
    fn send(&self, frame: &Frame) -> Result<(), CommandError> {
        self.write_tx
            .try_send(frame.encode(self.key.as_ref()))
            .map_err(|_| CommandError::ConnectionLost)
    }

    fn outbound(&self, sequence: u32, major: u8, minor: u8, payload: Vec<u8>) -> Frame {
        Frame {
            flags: FLAG_FROM_RECEIVER,
            sequence,
            src_id: self.receiver_id,
            dst_id: self.panel_id,
            major,
            minor,
            payload,
        }
    }

    /// Echo a panel-initiated frame back as its acknowledgement.
    fn ack(&self, req: &Frame, ack_minor: u8) -> Result<(), CommandError> {
        self.send(&self.outbound(req.sequence, req.major, ack_minor, req.payload.clone()))
    }

    fn close(&self) {
        let mut link = self.link.lock().unwrap();
        link.closed = true;
        // Dropping the senders fails every in-flight command immediately.
        link.pending.clear();
    }
}

/// Cheap handle for issuing commands on one panel connection.
#[derive(Clone)]
pub struct SessionHandle {
    id: SessionId,
    shared: Arc<Shared>,
}

impl fmt::Debug for SessionHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SessionHandle({}, panel {})",
            self.id, self.shared.panel_id
        )
    }
}

impl SessionHandle {
    pub fn id(&self) -> SessionId {
        self.id
    }

    pub async fn xml_query(&self, command_id: &str) -> Result<XmlReply, CommandError> {
        let _guard = self.shared.command_lock.lock().await;
        let mut assembler = ReplyAssembler::default();
        for fragment in 0..MAX_XML_FRAGMENTS {
            let payload = build_request(command_id, fragment > 0);
            let reply = self.transact(major::XML_CMD, payload, command_id).await?;
            if let Some(xml) = assembler.feed(&reply).map_err(CommandError::Protocol)? {
                return parse_reply(&xml).map_err(CommandError::Protocol);
            }
        }
        Err(CommandError::Protocol(format!(
            "reply to {command_id:?} did not close after {MAX_XML_FRAGMENTS} fragments"
        )))
    }

    pub async fn binary_command(&self, op: BinaryOp, target: u8) -> Result<(), CommandError> {
        let label = format!("{op:?} {target}");
        let _guard = self.shared.command_lock.lock().await;
        let reply = self
            .transact(major::BINARY_CMD, vec![op as u8, target, 0], &label)
            .await?;
        match reply.first() {
            Some(&REPLY_OK) => Ok(()),
            Some(&code) => Err(CommandError::Rejected {
                command: label,
                code,
            }),
            None => Err(CommandError::Protocol(format!("empty reply to {label}"))),
        }
    }

    /// One request/reply exchange. Caller must hold `command_lock`.
    async fn transact(
        &self,
        major: u8,
        payload: Vec<u8>,
        what: &str,
    ) -> Result<Vec<u8>, CommandError> {
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        let rx = loop {
            let quiet_at = {
                let mut link = self.shared.link.lock().unwrap();
                if link.closed {
                    return Err(CommandError::ConnectionLost);
                }
                let quiet_at = link.last_received + COMMAND_QUIET_TIME;
                if quiet_at <= Instant::now() {
                    link.next_seq = link.next_seq.wrapping_add(1);
                    let seq = link.next_seq;
                    let (tx, rx) = oneshot::channel();
                    link.pending.insert((major, seq), tx);
                    drop(link);
                    self.shared
                        .send(&self.shared.outbound(seq, major, minor::REQUEST, payload))?;
                    break (seq, rx);
                }
                quiet_at
            };
            if quiet_at > deadline {
                return Err(CommandError::Timeout(format!(
                    "the panel to go quiet before {what}"
                )));
            }
            tokio::time::sleep_until(quiet_at).await;
        };
        let (seq, rx) = rx;
        let result = tokio::time::timeout_at(deadline, rx).await;
        self.shared
            .link
            .lock()
            .unwrap()
            .pending
            .remove(&(major, seq));
        match result {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => Err(CommandError::ConnectionLost),
            Err(_) => Err(CommandError::Timeout(format!("a reply to {what}"))),
        }
    }
}

/// Accept panel connections forever. A new connection supersedes the
/// previous one: the panel keeps a single link per receiver, so a fresh dial
/// means the old socket is dead even if it has not timed out yet.
pub async fn run_receiver(
    config: ReceiverConfig,
    events: mpsc::Sender<LinkEvent>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(config.listen).await?;
    info!(
        "EDP receiver {} listening on {}",
        config.receiver_id, config.listen
    );
    let next_id = AtomicU64::new(1);
    let mut current: Option<(SessionId, oneshot::Sender<()>)> = None;
    loop {
        let (stream, peer) = listener.accept().await?;
        let id = SessionId(next_id.fetch_add(1, Ordering::Relaxed));
        if let Some((old_id, supersede)) = current.take()
            && supersede.send(()).is_ok()
        {
            warn!("Panel connection {id} from {peer} supersedes {old_id}");
        }
        info!("Panel connection {id} from {peer}");
        let (supersede_tx, supersede_rx) = oneshot::channel();
        tokio::spawn(run_session(
            id,
            stream,
            config.clone(),
            events.clone(),
            supersede_rx,
        ));
        current = Some((id, supersede_tx));
    }
}

async fn run_session(
    id: SessionId,
    stream: TcpStream,
    config: ReceiverConfig,
    events: mpsc::Sender<LinkEvent>,
    mut superseded: oneshot::Receiver<()>,
) {
    let (mut reader, writer) = stream.into_split();
    let (write_tx, write_rx) = mpsc::channel(WRITE_QUEUE_LEN);
    let writer_task = tokio::spawn(write_loop(writer, write_rx));

    let result = read_loop(id, &mut reader, &config, write_tx, &events, &mut superseded).await;
    writer_task.abort();
    match result {
        Ok(()) => info!("Panel connection {id} closed"),
        Err(e) => warn!("Panel connection {id} dropped: {e}"),
    }
    let _ = events.send(LinkEvent::Closed(id)).await;
}

async fn write_loop(mut writer: OwnedWriteHalf, mut rx: mpsc::Receiver<Vec<u8>>) {
    while let Some(data) = rx.recv().await {
        if let Err(e) = writer.write_all(&data).await {
            warn!("EDP write failed: {e}");
            return;
        }
    }
}

async fn read_loop(
    id: SessionId,
    reader: &mut tokio::net::tcp::OwnedReadHalf,
    config: &ReceiverConfig,
    write_tx: mpsc::Sender<Vec<u8>>,
    events: &mpsc::Sender<LinkEvent>,
    superseded: &mut oneshot::Receiver<()>,
) -> Result<(), String> {
    let mut decoder = FrameDecoder::new(config.key.clone());
    let mut buf = vec![0u8; READ_BUF_LEN];
    let mut shared: Option<Arc<Shared>> = None;
    let mut ready = false;

    let result = 'outer: loop {
        let read = tokio::select! {
            read = tokio::time::timeout(config.idle_timeout, reader.read(&mut buf)) => read,
            _ = &mut *superseded => break Err("superseded by a new panel connection".into()),
        };
        let n = match read {
            Err(_) => break Err(format!("no data for {:?}", config.idle_timeout)),
            Ok(Err(e)) => break Err(e.to_string()),
            Ok(Ok(0)) => break Ok(()),
            Ok(Ok(n)) => n,
        };
        let frames = match decoder.feed(&buf[..n]) {
            Ok(frames) => frames,
            Err(e @ DecodeError::EncryptionRequired) | Err(e @ DecodeError::Corrupt(_)) => {
                error!("{e}");
                break Err(e.to_string());
            }
        };
        for frame in frames {
            let sess = shared.get_or_insert_with(|| {
                info!("Panel {} connected ({id})", frame.src_id);
                Arc::new(Shared {
                    receiver_id: config.receiver_id,
                    panel_id: frame.src_id,
                    key: config.key.clone(),
                    write_tx: write_tx.clone(),
                    link: StdMutex::new(LinkState {
                        next_seq: 0,
                        last_received: Instant::now(),
                        pending: HashMap::new(),
                        closed: false,
                    }),
                    command_lock: Mutex::new(()),
                })
            });
            if frame.dst_id != config.receiver_id {
                warn!(
                    "Frame addressed to receiver {} (we are {})",
                    frame.dst_id, config.receiver_id
                );
            }
            if let Err(e) = dispatch(sess, &frame, id, events) {
                break 'outer Err(format!("cannot answer panel: {e}"));
            }
            if !ready && frame.major == major::SESSION && frame.minor == minor::POLL {
                ready = true;
                let handle = SessionHandle {
                    id,
                    shared: Arc::clone(sess),
                };
                if events.send(LinkEvent::Ready(handle)).await.is_err() {
                    break 'outer Ok(());
                }
            }
        }
    };
    if let Some(sess) = &shared {
        sess.close();
    }
    result
}

fn dispatch(
    sess: &Shared,
    frame: &Frame,
    id: SessionId,
    events: &mpsc::Sender<LinkEvent>,
) -> Result<(), CommandError> {
    {
        let mut link = sess.link.lock().unwrap();
        link.last_received = Instant::now();
        link.next_seq = link.next_seq.max(frame.sequence);
        let reply_major = match (frame.major, frame.minor) {
            (major::XML_CMD, minor::XML_REPLY) => Some(major::XML_CMD),
            (major::BINARY_CMD, minor::BINARY_REPLY) => Some(major::BINARY_CMD),
            _ => None,
        };
        if let Some(m) = reply_major {
            match link.pending.remove(&(m, frame.sequence)) {
                Some(tx) => {
                    let _ = tx.send(frame.payload.clone());
                }
                None => debug!(
                    "Unsolicited reply {}.{} seq {}",
                    frame.major, frame.minor, frame.sequence
                ),
            }
            return Ok(());
        }
    }

    match (frame.major, frame.minor) {
        (major::SESSION, minor::POLL) => sess.ack(frame, minor::POLL_ACK),
        (major::SESSION, minor::HELLO) => sess.ack(frame, minor::HELLO_ACK),
        (major::EVENT, minor::EVENT_PUSH) => {
            sess.ack(frame, minor::EVENT_ACK)?;
            match SiaEvent::parse(&frame.payload) {
                Ok(ev) => {
                    if events.try_send(LinkEvent::Sia(id, ev)).is_err() {
                        warn!("Link event queue full; dropping SIA event");
                    }
                }
                Err(e) => warn!("{e}"),
            }
            Ok(())
        }
        (m, n) => {
            debug!(
                "Unhandled EDP frame {m}.{n} ({} payload bytes)",
                frame.payload.len()
            );
            Ok(())
        }
    }
}
