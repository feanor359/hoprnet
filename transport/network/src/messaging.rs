use hopr_crypto_random::random_fill;
use hopr_crypto_types::prelude::blake3_hash;
use hopr_primitive_types::{errors::GeneralError, prelude::BytesEncodable};
use serde::{Deserialize, Serialize};

use crate::errors::{NetworkingError::MessagingError, Result};

/// Size of the nonce in the Ping sub protocol
pub const PING_PONG_NONCE_SIZE: usize = 16;

/// Derives a ping challenge (if no challenge is given) or a pong response to a ping challenge.
pub fn derive_ping_pong(challenge: Option<&[u8]>) -> [u8; PING_PONG_NONCE_SIZE] {
    let mut ret = [0u8; PING_PONG_NONCE_SIZE];
    match challenge {
        None => random_fill(&mut ret),
        Some(chal) => {
            let hash = blake3_hash(chal);
            ret.copy_from_slice(&hash.as_bytes()[0..PING_PONG_NONCE_SIZE]);
        }
    }
    ret
}

/// Implementation of the Control Message sub-protocol, which currently consists of Ping/Pong
/// messaging for the HOPR protocol.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ControlMessage {
    /// Ping challenge
    Ping(PingMessage),
    /// Pong response to a Ping
    Pong(PingMessage),
}

impl ControlMessage {
    /// Creates a ping challenge message.
    pub fn generate_ping_request() -> Self {
        let mut ping = PingMessage::default();
        ping.nonce.copy_from_slice(&derive_ping_pong(None));

        Self::Ping(ping)
    }

    /// Given the received ping challenge, generates an appropriate response to the challenge.
    pub fn generate_pong_response(request: &ControlMessage) -> Result<Self> {
        match request {
            ControlMessage::Ping(ping) => {
                let mut pong = PingMessage::default();
                pong.nonce.copy_from_slice(&derive_ping_pong(Some(ping.nonce())));
                Ok(Self::Pong(pong))
            }
            ControlMessage::Pong(_) => Err(MessagingError("invalid ping message".into())),
        }
    }

    /// Given the original ping challenge message, verifies that the received pong response is valid
    /// according to the challenge.
    pub fn validate_pong_response(request: &ControlMessage, response: &ControlMessage) -> Result<()> {
        if let Self::Pong(expected_pong) = Self::generate_pong_response(request).unwrap() {
            match response {
                ControlMessage::Pong(received_pong) => match expected_pong.nonce.eq(&received_pong.nonce) {
                    true => Ok(()),
                    false => Err(MessagingError("pong response does not match the challenge".into())),
                },
                ControlMessage::Ping(_) => Err(MessagingError("invalid pong response".into())),
            }
        } else {
            Err(MessagingError("request is not a valid ping message".into()))
        }
    }

    /// Convenience method to de-structure the ping message payload, if this
    /// instance is a Ping or Pong.
    pub fn get_ping_message(&self) -> Result<&PingMessage> {
        match self {
            ControlMessage::Ping(m) | ControlMessage::Pong(m) => Ok(m),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Default, Serialize, Deserialize)]
pub struct PingMessage {
    nonce: [u8; PING_PONG_NONCE_SIZE],
}

impl PingMessage {
    /// Gets the challenge or response in this ping/pong message.
    pub fn nonce(&self) -> &[u8] {
        &self.nonce
    }
}

impl From<PingMessage> for [u8; PING_MESSAGE_LEN] {
    fn from(value: PingMessage) -> Self {
        let mut ret = [0u8; PING_MESSAGE_LEN];
        ret[0..PING_MESSAGE_LEN].copy_from_slice(&value.nonce);
        ret
    }
}

impl TryFrom<&[u8]> for PingMessage {
    type Error = GeneralError;

    fn try_from(value: &[u8]) -> std::result::Result<Self, Self::Error> {
        if value.len() >= Self::SIZE {
            let mut ret = PingMessage::default();
            ret.nonce.copy_from_slice(&value[0..Self::SIZE]);
            Ok(ret)
        } else {
            Err(GeneralError::ParseError("PingMessage".into()))
        }
    }
}

const PING_MESSAGE_LEN: usize = PING_PONG_NONCE_SIZE;
impl BytesEncodable<PING_MESSAGE_LEN> for PingMessage {}

#[cfg(test)]
mod tests {
    use hopr_primitive_types::prelude::BytesEncodable;

    use crate::messaging::{
        ControlMessage,
        ControlMessage::{Ping, Pong},
        PingMessage,
    };

    #[test]
    fn test_ping_pong_roundtrip() {
        // ping initiator
        let sent_req_s: Box<[u8]>;
        let sent_req: ControlMessage;
        {
            sent_req = ControlMessage::generate_ping_request();
            sent_req_s = sent_req.get_ping_message().unwrap().clone().into_boxed();
        }

        // pong responder
        let sent_resp_s: Box<[u8]>;
        {
            let recv_req = PingMessage::try_from(sent_req_s.as_ref()).unwrap();
            let send_resp = ControlMessage::generate_pong_response(&Ping(recv_req)).unwrap();
            sent_resp_s = send_resp.get_ping_message().unwrap().clone().into_boxed();
        }

        // verify pong
        {
            let recv_resp = PingMessage::try_from(sent_resp_s.as_ref()).unwrap();
            assert!(ControlMessage::validate_pong_response(&sent_req, &Pong(recv_resp)).is_ok());
        }
    }
}
