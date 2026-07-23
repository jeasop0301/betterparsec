//! Pure wire codecs and ownership state. This module intentionally has no Windows API surface.

pub const VERSION: u16 = 1;
pub const PIPE_MAGIC: [u8; 4] = *b"BPK1";
pub const MAX_PAYLOAD: usize = 64 * 1024;
pub const EVENT_SIZE: usize = 32;
pub const BATCH_PREFIX_SIZE: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum MessageKind {
    Arm = 1,
    Disarm = 2,
    Events = 3,
    Status = 4,
}

impl TryFrom<u16> for MessageKind {
    type Error = ProtocolError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Arm),
            2 => Ok(Self::Disarm),
            3 => Ok(Self::Events),
            4 => Ok(Self::Status),
            _ => Err(ProtocolError::UnknownKind),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub kind: MessageKind,
    pub payload_len: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub kind: u16,
    pub sequence: u32,
    pub nonce: u64,
    pub make_code: u16,
    pub flags: u16,
    pub extra_information: u32,
    pub interrupt_time_100ns: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Batch {
    pub dropped: u32,
    pub events: Vec<Event>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolError {
    Truncated,
    BadMagic,
    VersionMismatch,
    UnknownKind,
    PayloadTooLarge,
    MalformedBatch,
    SequenceGap,
    AlreadyConnected,
    NotOwner,
    NotArmed,
    NonceMismatch,
}

pub fn encode_header(kind: MessageKind, payload_len: usize) -> Result<[u8; 12], ProtocolError> {
    if payload_len > MAX_PAYLOAD {
        return Err(ProtocolError::PayloadTooLarge);
    }
    let mut bytes = [0; 12];
    bytes[..4].copy_from_slice(&PIPE_MAGIC);
    bytes[4..6].copy_from_slice(&VERSION.to_le_bytes());
    bytes[6..8].copy_from_slice(&(kind as u16).to_le_bytes());
    bytes[8..12].copy_from_slice(&(payload_len as u32).to_le_bytes());
    Ok(bytes)
}

pub fn decode_header(bytes: &[u8]) -> Result<Header, ProtocolError> {
    if bytes.len() != 12 {
        return Err(ProtocolError::Truncated);
    }
    if bytes[..4] != PIPE_MAGIC {
        return Err(ProtocolError::BadMagic);
    }
    if u16::from_le_bytes([bytes[4], bytes[5]]) != VERSION {
        return Err(ProtocolError::VersionMismatch);
    }
    let payload_len = u32::from_le_bytes(bytes[8..12].try_into().expect("fixed header"));
    if payload_len as usize > MAX_PAYLOAD {
        return Err(ProtocolError::PayloadTooLarge);
    }
    Ok(Header {
        kind: u16::from_le_bytes([bytes[6], bytes[7]]).try_into()?,
        payload_len,
    })
}

pub fn decode_batch(bytes: &[u8]) -> Result<Batch, ProtocolError> {
    if bytes.len() < BATCH_PREFIX_SIZE {
        return Err(ProtocolError::MalformedBatch);
    }
    let count = u32::from_le_bytes(bytes[..4].try_into().expect("batch prefix")) as usize;
    let dropped = u32::from_le_bytes(bytes[4..8].try_into().expect("batch prefix"));
    let expected = BATCH_PREFIX_SIZE
        .checked_add(
            count
                .checked_mul(EVENT_SIZE)
                .ok_or(ProtocolError::MalformedBatch)?,
        )
        .ok_or(ProtocolError::MalformedBatch)?;
    if bytes.len() != expected || bytes.len() > MAX_PAYLOAD {
        return Err(ProtocolError::MalformedBatch);
    }

    let mut events = Vec::with_capacity(count);
    for chunk in bytes[BATCH_PREFIX_SIZE..].chunks_exact(EVENT_SIZE) {
        if u16::from_le_bytes(chunk[..2].try_into().expect("event version")) != VERSION {
            return Err(ProtocolError::VersionMismatch);
        }
        events.push(Event {
            kind: u16::from_le_bytes(chunk[2..4].try_into().expect("event kind")),
            sequence: u32::from_le_bytes(chunk[4..8].try_into().expect("event sequence")),
            nonce: u64::from_le_bytes(chunk[8..16].try_into().expect("event nonce")),
            make_code: u16::from_le_bytes(chunk[16..18].try_into().expect("make code")),
            flags: u16::from_le_bytes(chunk[18..20].try_into().expect("flags")),
            extra_information: u32::from_le_bytes(chunk[20..24].try_into().expect("extra")),
            interrupt_time_100ns: u64::from_le_bytes(chunk[24..32].try_into().expect("time")),
        });
    }
    Ok(Batch { dropped, events })
}

pub fn encode_batch(batch: &Batch) -> Result<Vec<u8>, ProtocolError> {
    let body_len = batch
        .events
        .len()
        .checked_mul(EVENT_SIZE)
        .ok_or(ProtocolError::PayloadTooLarge)?;
    let total_len = BATCH_PREFIX_SIZE
        .checked_add(body_len)
        .ok_or(ProtocolError::PayloadTooLarge)?;
    if total_len > MAX_PAYLOAD {
        return Err(ProtocolError::PayloadTooLarge);
    }
    let mut bytes = Vec::with_capacity(total_len);
    bytes.extend_from_slice(&(batch.events.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&batch.dropped.to_le_bytes());
    for event in &batch.events {
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&event.kind.to_le_bytes());
        bytes.extend_from_slice(&event.sequence.to_le_bytes());
        bytes.extend_from_slice(&event.nonce.to_le_bytes());
        bytes.extend_from_slice(&event.make_code.to_le_bytes());
        bytes.extend_from_slice(&event.flags.to_le_bytes());
        bytes.extend_from_slice(&event.extra_information.to_le_bytes());
        bytes.extend_from_slice(&event.interrupt_time_100ns.to_le_bytes());
    }
    Ok(bytes)
}

#[derive(Default, Debug)]
pub struct CaptureState {
    owner: Option<u64>,
    armed: bool,
    last_sequence: Option<u32>,
    nonce: Option<u64>,
}

impl CaptureState {
    pub fn connect(&mut self, client: u64) -> Result<(), ProtocolError> {
        if self.owner.is_some() {
            return Err(ProtocolError::AlreadyConnected);
        }
        self.owner = Some(client);
        self.armed = false;
        self.last_sequence = None;
        self.nonce = None;
        Ok(())
    }

    pub fn arm(&mut self, client: u64, nonce: u64) -> Result<(), ProtocolError> {
        self.require_owner(client)?;
        self.armed = true;
        self.last_sequence = None;
        self.nonce = Some(nonce);
        Ok(())
    }

    pub fn disarm(&mut self, client: u64) -> Result<(), ProtocolError> {
        self.require_owner(client)?;
        self.armed = false;
        self.last_sequence = None;
        self.nonce = None;
        Ok(())
    }

    pub fn disconnect(&mut self, client: u64) -> Result<(), ProtocolError> {
        self.require_owner(client)?;
        self.owner = None;
        self.armed = false;
        self.last_sequence = None;
        self.nonce = None;
        Ok(())
    }

    pub fn fail_closed(&mut self) {
        self.armed = false;
        self.last_sequence = None;
        self.nonce = None;
    }

    pub fn is_armed(&self) -> bool {
        self.owner.is_some() && self.armed
    }

    pub fn observe_batch(&mut self, client: u64, batch: &Batch) -> Result<(), ProtocolError> {
        self.require_owner(client)?;
        if !self.armed {
            return Err(ProtocolError::NotArmed);
        }
        if batch.dropped != 0 {
            self.fail_closed();
            return Err(ProtocolError::SequenceGap);
        }
        for event in &batch.events {
            if Some(event.nonce) != self.nonce {
                self.fail_closed();
                return Err(ProtocolError::NonceMismatch);
            }
            if self.last_sequence.is_none() && event.sequence != 1 {
                self.fail_closed();
                return Err(ProtocolError::SequenceGap);
            }
            if let Some(last) = self.last_sequence
                && event.sequence != last.wrapping_add(1)
            {
                self.fail_closed();
                return Err(ProtocolError::SequenceGap);
            }
            self.last_sequence = Some(event.sequence);
        }
        Ok(())
    }

    fn require_owner(&self, client: u64) -> Result<(), ProtocolError> {
        if self.owner == Some(client) {
            Ok(())
        } else {
            Err(ProtocolError::NotOwner)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(sequence: u32) -> Event {
        Event {
            kind: 1,
            sequence,
            nonce: 2,
            make_code: 3,
            flags: 4,
            extra_information: 5,
            interrupt_time_100ns: 6,
        }
    }

    #[test]
    fn versioning_rejects_mismatch() {
        let mut header = encode_header(MessageKind::Arm, 0).expect("valid header");
        header[4] = 2;
        assert_eq!(decode_header(&header), Err(ProtocolError::VersionMismatch));
    }

    #[test]
    fn lease_requires_an_armed_owner() {
        let mut state = CaptureState::default();
        state.connect(7).expect("owner connects");
        assert!(!state.is_armed());
        state.arm(7, 2).expect("owner arms");
        assert!(state.is_armed());
        state.disarm(7).expect("owner disarms");
        assert!(!state.is_armed());
    }

    #[test]
    fn only_one_client_can_own_capture() {
        let mut state = CaptureState::default();
        state.connect(1).expect("owner connects");
        assert_eq!(state.connect(2), Err(ProtocolError::AlreadyConnected));
        assert_eq!(state.arm(2, 2), Err(ProtocolError::NotOwner));
    }

    #[test]
    fn disconnect_disarms_capture() {
        let mut state = CaptureState::default();
        state.connect(1).expect("owner connects");
        state.arm(1, 2).expect("owner arms");
        state.disconnect(1).expect("owner disconnects");
        assert!(!state.is_armed());
    }

    #[test]
    fn sequence_gap_fails_closed() {
        let mut state = CaptureState::default();
        state.connect(1).expect("owner connects");
        state.arm(1, 2).expect("owner arms");
        state
            .observe_batch(
                1,
                &Batch {
                    dropped: 0,
                    events: vec![event(1)],
                },
            )
            .expect("first batch is contiguous");
        assert_eq!(
            state.observe_batch(
                1,
                &Batch {
                    dropped: 0,
                    events: vec![event(3)]
                }
            ),
            Err(ProtocolError::SequenceGap)
        );
        assert!(!state.is_armed());
    }

    #[test]
    fn dropped_driver_events_fail_closed() {
        let mut state = CaptureState::default();
        state.connect(1).expect("owner connects");
        state.arm(1, 2).expect("owner arms");
        let batch = Batch {
            dropped: 1,
            events: vec![event(1)],
        };
        assert_eq!(
            state.observe_batch(1, &batch),
            Err(ProtocolError::SequenceGap)
        );
        assert!(!state.is_armed());
    }

    #[test]
    fn nonce_mismatch_fails_closed() {
        let mut state = CaptureState::default();
        state.connect(1).expect("owner connects");
        state.arm(1, 9).expect("owner arms");
        assert_eq!(
            state.observe_batch(
                1,
                &Batch {
                    dropped: 0,
                    events: vec![event(1)]
                }
            ),
            Err(ProtocolError::NonceMismatch)
        );
        assert!(!state.is_armed());
    }

    #[test]
    fn new_capture_epoch_must_start_at_sequence_one() {
        let mut state = CaptureState::default();
        state.connect(1).expect("owner connects");
        state.arm(1, 2).expect("owner arms");
        assert_eq!(
            state.observe_batch(
                1,
                &Batch {
                    dropped: 0,
                    events: vec![event(2)]
                }
            ),
            Err(ProtocolError::SequenceGap)
        );
        assert!(!state.is_armed());
    }
    #[test]
    fn malformed_batch_is_rejected() {
        assert_eq!(decode_batch(&[0; 7]), Err(ProtocolError::MalformedBatch));
        let mut batch = encode_batch(&Batch {
            dropped: 0,
            events: vec![event(1)],
        })
        .expect("valid batch");
        batch.pop();
        assert_eq!(decode_batch(&batch), Err(ProtocolError::MalformedBatch));
    }
}
