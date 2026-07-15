use commonware_codec::{Error, FixedSize, Read, ReadExt as _, Write};
use commonware_runtime::{Buf, BufMut};

const HEARTBEAT_TAG: u8 = 0;
const HEARTBEAT_ACK_TAG: u8 = 1;

/// PoC messages exchanged on the P2P control channel.
///
/// TODO: This heartbeat exchange exists only to exercise authenticated
/// message transport. Replace it with the v0 block, ACK/signature, transaction-forwarding,
/// and backfill protocols. It is not a leader-election heartbeat or a source of leadership
/// or finality, and will be removed with the next PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlMessage {
    Heartbeat { nonce: u64 },
    HeartbeatAck { nonce: u64 },
}

impl Write for ControlMessage {
    fn write(&self, buffer: &mut impl BufMut) {
        match self {
            Self::Heartbeat { nonce } => {
                HEARTBEAT_TAG.write(buffer);
                nonce.write(buffer);
            }
            Self::HeartbeatAck { nonce } => {
                HEARTBEAT_ACK_TAG.write(buffer);
                nonce.write(buffer);
            }
        }
    }
}

impl FixedSize for ControlMessage {
    const SIZE: usize = u8::SIZE + u64::SIZE;
}

impl Read for ControlMessage {
    type Cfg = ();

    fn read_cfg(buffer: &mut impl Buf, _cfg: &Self::Cfg) -> Result<Self, Error> {
        let tag = u8::read(buffer)?;
        let nonce = u64::read(buffer)?;
        match tag {
            HEARTBEAT_TAG => Ok(Self::Heartbeat { nonce }),
            HEARTBEAT_ACK_TAG => Ok(Self::HeartbeatAck { nonce }),
            other => Err(Error::InvalidEnum(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use commonware_codec::{DecodeExt as _, Encode as _};

    use super::ControlMessage;

    #[test]
    fn control_messages_round_trip() {
        for message in [
            ControlMessage::Heartbeat { nonce: 42 },
            ControlMessage::HeartbeatAck { nonce: u64::MAX },
        ] {
            assert_eq!(ControlMessage::decode(message.encode()).unwrap(), message);
        }
    }
}
