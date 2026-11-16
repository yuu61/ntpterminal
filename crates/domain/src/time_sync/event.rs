use alloc::fmt;

use super::{KissCode, Sample, SampleError, ServerInfo, SyncState};
use crate::shared_kernel::TimeDelta;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// 応答待ちの問い合わせがないのに受信した
    NoPendingExchange,
    /// 応答待ちの問い合わせと Originate Timestamp が一致しない（遅れて届いた応答・無関係なパケット）
    OriginMismatch,
    /// NTP パケットとして解析できない
    Malformed,
    UnexpectedMode(u8),
    UnsupportedVersion(u8),
    KissOfDeath(KissCode),
    /// サーバ自身が同期していない（LI=3 または stratum 16）
    ServerUnsynchronized,
    InvalidStratum(u8),
    ZeroTransmitTimestamp,
    InvalidSample(SampleError),
}

impl RejectReason {
    /// 自分の問い合わせとは関係のない受信か（問い合わせの成否には数えない）。
    #[must_use]
    pub const fn is_stray(self) -> bool {
        matches!(
            self,
            Self::NoPendingExchange | Self::OriginMismatch | Self::Malformed
        )
    }
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::NoPendingExchange => f.write_str("応答待ちの問い合わせがないときに受信しました"),
            Self::OriginMismatch => f.write_str(
                "応答待ちの問い合わせと一致しません（遅れて届いた応答か、無関係なパケット）",
            ),
            Self::Malformed => f.write_str("NTP パケットとして解析できません"),
            Self::UnexpectedMode(mode) => write!(f, "サーバの応答ではありません（mode {mode}）"),
            Self::UnsupportedVersion(version) => {
                write!(f, "対応していない NTP バージョンです（{version}）")
            }
            Self::KissOfDeath(code) => {
                write!(f, "サーバから Kiss-o'-Death を受け取りました（{code}）")
            }
            Self::ServerUnsynchronized => {
                f.write_str("サーバ自身が同期していません（LI=3 または stratum 16）")
            }
            Self::InvalidStratum(stratum) => write!(f, "stratum が範囲外です（{stratum}）"),
            Self::ZeroTransmitTimestamp => f.write_str("サーバの送信時刻が 0 です"),
            Self::InvalidSample(error) => write!(f, "{error}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncEvent {
    ExchangeStarted {
        seq: u64,
    },
    SampleAccepted {
        seq: u64,
        sample: Sample,
        server_info: ServerInfo,
    },
    /// `seq` が `None` なら、自分の問い合わせとは関係のない受信。
    SampleRejected {
        seq: Option<u64>,
        reason: RejectReason,
    },
    ExchangeTimedOut {
        seq: u64,
    },
    /// 送受信のエラー（到達不能など）。`seq` が `None` なら応答待ちの問い合わせはなかった。
    ExchangeFailed {
        seq: Option<u64>,
        detail: String,
    },
    PollIntervalChanged {
        from: TimeDelta,
        to: TimeDelta,
    },
    StateChanged {
        from: SyncState,
        to: SyncState,
    },
}
