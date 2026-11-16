//! 腐敗防止層（ACL）: パケット生成コンテキストの [`NtpMessage`] を、
//! 時刻同期コンテキストの [`ServerReply`] に翻訳する。
//!
//! 翻訳だけを行い、検証はしない（検証は時刻同期コンテキストの責務）。
//! パケット生成側のモデルが変わっても、影響はここで止まる。

use domain::packet_crafting::NtpMessage;
use domain::time_sync::ServerReply;

pub fn to_server_reply(message: &NtpMessage) -> ServerReply {
    ServerReply {
        leap: message.leap,
        version: message.version,
        mode: message.mode,
        stratum: message.stratum,
        poll: message.poll,
        precision: message.precision,
        root_delay: message.root_delay,
        root_dispersion: message.root_dispersion,
        reference_id: message.reference_id,
        reference_time: message.reference_timestamp,
        origin: message.origin_timestamp,
        receive: message.receive_timestamp,
        transmit: message.transmit_timestamp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use domain::shared_kernel::NtpTimestamp;

    #[test]
    fn maps_timestamps_to_their_roles() {
        let message = NtpMessage {
            reference_timestamp: NtpTimestamp::from_bits(1),
            origin_timestamp: NtpTimestamp::from_bits(2),
            receive_timestamp: NtpTimestamp::from_bits(3),
            transmit_timestamp: NtpTimestamp::from_bits(4),
            reference_id: *b"GNSS",
            stratum: 1,
            mode: 4,
            ..NtpMessage::default()
        };
        let reply = to_server_reply(&message);
        assert_eq!(reply.reference_time, NtpTimestamp::from_bits(1));
        assert_eq!(reply.origin, NtpTimestamp::from_bits(2));
        assert_eq!(reply.receive, NtpTimestamp::from_bits(3));
        assert_eq!(reply.transmit, NtpTimestamp::from_bits(4));
        assert_eq!(&reply.reference_id, b"GNSS");
    }
}
