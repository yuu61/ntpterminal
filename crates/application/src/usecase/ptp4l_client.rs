//! ptp4l への問い合わせ: 管理用ソケット（Unix ドメインソケット）で GET を送り、応答を待つ。
//!
//! 決まりごと（理由は linuxptp の `clock.c`・`port.c`・`pmc_common.c` の扱いから）:
//! - 送受信は [`PacketPort`]（宛先は ptp4l の管理用ソケット）。メッセージの読み書きは domain の
//!   `ptp_management`、値の解釈は ACL（[`crate::acl::ptp4l`]）
//! - 送るのは `pmc -u -b 0 -z` と同じ GET。boundaryHops を 0 にして、ptp4l がネットワークへ転送しないようにする
//!   （`pmc` の既定の 1 だと、ptp4l は問い合わせを PTP のポートからも送り出す）
//! - 送り元の識別子は `pmc` と同じくプロセス番号から作る（ptp4l は、自分と同じ識別子のメッセージを捨てる）
//! - ptp4l はドメイン番号と transportSpecific が自分の設定と違うメッセージを、管理用ソケットでも黙って捨てる。
//!   応答がなければドメイン番号の違いを疑うよう知らせる。transportSpecific は 0 で送るので、gPTP の設定
//!   （transportSpecific 1）の ptp4l には答えてもらえない
//! - 応答は sequenceId と managementId で照合する。時間切れの後に届いた前の問い合わせの応答は捨てる
//! - ポートの一覧（[`Ptp4lClient::discover_ports`]）で使う `PORT_PROPERTIES_NP`・`PORT_HWCLOCK_NP` には、
//!   ptp4l は書き込みもできる側のソケット（`uds_address`）でしか答えない（読み取り専用の `uds_ro_address` では
//!   `NOT_SUPPORTED`）。`PORT_HWCLOCK_NP` は linuxptp 4.0 からなので、エラーが返れば「分からない」とする
//! - 状態の読み取り（[`Ptp4lClient::status`]）は、1 つでも失敗したら残りを問い合わせずにやめる
//!   （ptp4l が止まっているときに、問い合わせの数だけ時間切れを待たないため）
//! - 応答を待つ間も、100ms ごとに停止の合図を確かめる

use alloc::sync::Arc;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;

use domain::ptp_clock::{PortState, PtpStatus};
use domain::ptp_management::{
    Action, CurrentDataSet, DecodeError, DefaultDataSet, ManagementErrorId, ManagementId,
    ManagementMessage, ManagementTlv, ParentDataSet, PortDataSet, PortHwclockNp, PortIdentity,
    PortPropertiesNp, TimePropertiesDataSet, decode, encode,
};
use domain::shared_kernel::{LocalStamp, TimeDelta};

use crate::acl::ptp4l::to_status;
use crate::port::{ClockPort, PacketPort, TransportError};

/// 応答を待つとき、停止の合図を確かめる間隔。
const STOP_CHECK_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ptp4lClientConfig {
    /// ptp4l のドメイン番号（ptp4l の `domainNumber`）
    pub domain: u8,
    /// 1 回の GET で応答を待つ上限
    pub timeout: Duration,
    /// 送り元の識別子を作る番号（プロセス番号。`pmc` と同じ）
    pub client_id: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Ptp4lError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(
        "ptp4l から {id} の応答がありません（ptp4l が動いているか、ドメイン番号が ptp4l の設定と同じか確認してください）"
    )]
    Timeout { id: ManagementId },
    #[error("ptp4l が {id} の問い合わせにエラーを返しました（{error}{detail}）")]
    Refused {
        id: ManagementId,
        error: ManagementErrorId,
        /// ptp4l が添えた説明（"、…" の形。なければ空）
        detail: String,
    },
    #[error(
        "ptp4l がポートの一覧に答えません（読み取り専用のソケット uds_ro_address では答えないので、uds_address のソケットを指定してください）"
    )]
    ReadOnlySocket,
    #[error("ptp4l の {id} の応答を読めません: {source}")]
    Malformed {
        id: ManagementId,
        source: DecodeError,
    },
    /// 停止の合図で、応答を待つのをやめた
    #[error("停止しました")]
    Stopped,
}

/// ポートのタイムスタンプの方式（linuxptp の `enum timestamp_type`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortTimestamping {
    Software,
    Hardware,
    LegacyHardware,
    OneStep,
    P2pOneStep,
    Unknown(u8),
}

impl PortTimestamping {
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        match bits {
            0 => Self::Software,
            1 => Self::Hardware,
            2 => Self::LegacyHardware,
            3 => Self::OneStep,
            4 => Self::P2pOneStep,
            other => Self::Unknown(other),
        }
    }

    /// PHC を使う方式か（ソフトウェアと従来の方式のハードウェアのタイムスタンプでは、ptp4l は
    /// システムの時計を合わせる。linuxptp の `clock_create` で PHC の番号が -1 になる）。
    #[must_use]
    pub const fn uses_phc(self) -> bool {
        self.without_phc_label().is_none()
    }

    /// PHC を使わない方式の説明（エラーの文言）。PHC を使う方式なら `None`。
    #[must_use]
    pub const fn without_phc_label(self) -> Option<&'static str> {
        match self {
            Self::Software => Some("ソフトウェアのタイムスタンプ（-S）"),
            Self::LegacyHardware => Some("従来の方式のハードウェアのタイムスタンプ（-L）"),
            Self::Hardware | Self::OneStep | Self::P2pOneStep | Self::Unknown(_) => None,
        }
    }
}

/// ポートが使う PHC（`PORT_HWCLOCK_NP` の値）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HwClock {
    /// /dev/ptpN の N（負なら PHC を使っていない）
    pub index: i32,
    /// 仮想 PHC（vclock）か
    pub vclock: bool,
}

/// ptp4l のポート 1 つ（起動時に調べる）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ptp4lPort {
    pub number: u16,
    pub interface: String,
    pub timestamping: PortTimestamping,
    /// ptp4l が知らせた PHC。linuxptp 4.0 より前は答えないので `None`
    pub hwclock: Option<HwClock>,
    pub state: PortState,
}

/// ptp4l の管理用ソケットへの問い合わせ。
pub struct Ptp4lClient<P> {
    port: P,
    clock: Arc<dyn ClockPort>,
    config: Ptp4lClientConfig,
    source: PortIdentity,
    last_sequence: u16,
    stop: Arc<AtomicBool>,
}

impl<P> fmt::Debug for Ptp4lClient<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ptp4lClient")
            .field("config", &self.config)
            .field("source", &self.source)
            .field("last_sequence", &self.last_sequence)
            .finish_non_exhaustive()
    }
}

impl<P: PacketPort> Ptp4lClient<P> {
    pub fn new(port: P, clock: Arc<dyn ClockPort>, config: Ptp4lClientConfig) -> Self {
        // pmc と同じ: clockIdentity の末尾 2 バイトにプロセス番号の上位、ポート番号に下位を入れる
        let [high, mid, low_high, low_low] = config.client_id.to_be_bytes();
        let source = PortIdentity {
            clock: [0, 0, 0, 0, 0, 0, high, mid],
            port: u16::from_be_bytes([low_high, low_low]),
        };
        Self {
            port,
            clock,
            config,
            source,
            last_sequence: 0,
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    pub const fn config(&self) -> &Ptp4lClientConfig {
        &self.config
    }

    /// 応答を待つ間に確かめる停止の合図。
    pub fn set_stop(&mut self, stop: Arc<AtomicBool>) {
        self.stop = stop;
    }

    /// ptp4l のポートの一覧（インターフェース名・タイムスタンプの方式・PHC・状態）。
    ///
    /// # Errors
    ///
    /// 問い合わせに失敗したとき（`PORT_HWCLOCK_NP` へのエラー応答は失敗にしない）。
    pub fn discover_ports(&mut self) -> Result<Vec<Ptp4lPort>, Ptp4lError> {
        let default = self.read(
            PortIdentity::WILDCARD,
            ManagementId::DEFAULT_DATA_SET,
            DefaultDataSet::parse,
        )?;
        let mut ports = Vec::with_capacity(usize::from(default.number_ports));
        for number in 1..=default.number_ports {
            let target = PortIdentity::port_of_any_clock(number);
            let properties = self
                .read(
                    target,
                    ManagementId::PORT_PROPERTIES_NP,
                    PortPropertiesNp::parse,
                )
                .map_err(|error| {
                    if matches!(
                        error,
                        Ptp4lError::Refused {
                            error: ManagementErrorId::NotSupported,
                            ..
                        }
                    ) {
                        Ptp4lError::ReadOnlySocket
                    } else {
                        error
                    }
                })?;
            let hwclock =
                match self.read(target, ManagementId::PORT_HWCLOCK_NP, PortHwclockNp::parse) {
                    Ok(hwclock) => Some(HwClock {
                        index: hwclock.phc_index,
                        vclock: hwclock.flags & PortHwclockNp::VCLOCK != 0,
                    }),
                    // linuxptp 4.0 より前は知らない ID としてエラーを返す
                    Err(Ptp4lError::Refused { .. }) => None,
                    Err(other) => return Err(other),
                };
            ports.push(Ptp4lPort {
                number,
                interface: String::from_utf8_lossy(&properties.interface).into_owned(),
                timestamping: PortTimestamping::from_bits(properties.timestamping),
                hwclock,
                state: PortState::from_bits(properties.port_state),
            });
        }
        Ok(ports)
    }

    /// 時計の状態と、`ports` のポートの状態を読む。1 つでも失敗したら、そこでやめる。
    ///
    /// # Errors
    ///
    /// 問い合わせのどれかに失敗したとき。
    pub fn status(&mut self, ports: &[u16]) -> Result<PtpStatus, Ptp4lError> {
        let current = self.read(
            PortIdentity::WILDCARD,
            ManagementId::CURRENT_DATA_SET,
            CurrentDataSet::parse,
        )?;
        let parent = self.read(
            PortIdentity::WILDCARD,
            ManagementId::PARENT_DATA_SET,
            ParentDataSet::parse,
        )?;
        let time = self.read(
            PortIdentity::WILDCARD,
            ManagementId::TIME_PROPERTIES_DATA_SET,
            TimePropertiesDataSet::parse,
        )?;
        let port_sets = ports
            .iter()
            .map(|&number| {
                self.read(
                    PortIdentity::port_of_any_clock(number),
                    ManagementId::PORT_DATA_SET,
                    PortDataSet::parse,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(to_status(
            self.config.domain,
            &current,
            &parent,
            &time,
            &port_sets,
        ))
    }

    /// GET を送り、応答のデータを `parse` で読む。
    fn read<T>(
        &mut self,
        target: PortIdentity,
        id: ManagementId,
        parse: fn(&[u8]) -> Result<T, DecodeError>,
    ) -> Result<T, Ptp4lError> {
        let data = self.get(target, id)?;
        parse(&data).map_err(|source| Ptp4lError::Malformed { id, source })
    }

    /// GET を送り、同じ sequenceId と managementId の応答を待って、データ部を返す。
    fn get(&mut self, target: PortIdentity, id: ManagementId) -> Result<Vec<u8>, Ptp4lError> {
        let sequence_id = self.next_sequence();
        let request =
            ManagementMessage::get(self.config.domain, self.source, sequence_id, target, id);
        let _: LocalStamp = self.port.send(&encode(&request))?;
        let deadline = self
            .clock
            .now()
            .mono
            .saturating_add(TimeDelta::from_std(self.config.timeout));
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Err(Ptp4lError::Stopped);
            }
            let remaining = deadline
                .since(self.clock.now().mono)
                .unwrap_or(TimeDelta::ZERO);
            if remaining <= TimeDelta::ZERO {
                return Err(Ptp4lError::Timeout { id });
            }
            let wait = remaining.to_std_saturating().min(STOP_CHECK_INTERVAL);
            let Some(inbound) = self.port.recv(wait)? else {
                continue;
            };
            // 読めないもの・前の問い合わせの応答・ほかの ID の応答は捨てて、待ち続ける
            let Ok(reply) = decode(&inbound.payload) else {
                continue;
            };
            if reply.sequence_id != sequence_id {
                continue;
            }
            match reply.tlv {
                ManagementTlv::Management { id: answered, data }
                    if answered == id && reply.action == Action::Response =>
                {
                    return Ok(data);
                }
                ManagementTlv::ErrorStatus {
                    error,
                    id: answered,
                    display,
                } if answered == id => {
                    let text = String::from_utf8_lossy(&display);
                    return Err(Ptp4lError::Refused {
                        id,
                        error,
                        detail: if text.is_empty() {
                            String::new()
                        } else {
                            format!("、{text}")
                        },
                    });
                }
                ManagementTlv::Management { .. }
                | ManagementTlv::ErrorStatus { .. }
                | ManagementTlv::Other { .. } => {}
            }
        }
    }

    /// 1 から順に使い、最後まで来たら 0 に戻る（sequenceId は 16 bit）。
    const fn next_sequence(&mut self) -> u16 {
        self.last_sequence = self.last_sequence.wrapping_add(1);
        self.last_sequence
    }
}
