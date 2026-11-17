//! 開発・テスト用の疑似 PTP マスタ。PTP のスレーブ（`--ptp`）を、実機のマスタなしで試すためのもの。
//!
//! - Announce（`log_announce_interval` ごと。Sync と一緒に送るので、Sync の間隔より短くはしない）と
//!   Sync（`log_sync_interval` ごと）、two-step なら `Follow_Up` を
//!   `destination` へ送り、
//!   `Delay_Req` に `Delay_Resp` で応える。Sync はイベント用のポート、ほかは一般用のポート（+1）へ送る
//! - 時刻は「PC の壁時計 + `offset`」の UTC に、UTC との差（37 秒）を足した TAI（時刻系は PTP、UTC との差は確か）。
//!   グランドマスタは、何にも合わせていない時計（clockClass 248・時刻源は内部の発振器・追跡可能でない）として知らせる。
//!   疑似マスタを使った表示が、GNSS に合わせた時計に見えないようにするため
//! - 時刻はユーザ空間で読む（送る直前・受け取った直後）ので、揺らぎは数十 µs ほどある
//! - ユニキャストの `Delay_Req`（フラグ付き）には、送り手の一般用のポート（送り手のポート + 1）へユニキャストで応える
//!   （logMessageInterval は 0x7F。ptp4l と同じ）。マルチキャストの `Delay_Req` にはマルチキャストで応える
//! - 宛先がマルチキャストのグループなら、`interface` のアドレスから TTL 1 で送り、イベント用のソケットをそのグループに
//!   `interface` で参加させて、マルチキャストの `Delay_Req` を受け取る。このときは全アドレスに bind する（Linux では、
//!   特定のアドレスに bind したソケットにはマルチキャストが届かない）。ユニキャストなら `interface` のアドレスに bind する
//!   （ループバックなら、Windows のファイアウォールの確認が出ない）
//! - 受信は無期限に待ち、止めるときは自分宛てに空のデータグラムを送って起こす（`sim_server` と同じ理由）。
//!   宛先は bind したアドレス（全アドレスなら 127.0.0.1）

use alloc::sync::Arc;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use std::io;
use std::net::UdpSocket;
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use domain::ptp_clock::log_interval;
use domain::ptp_message::{
    Announce, Body, Correction, Flags, Header, PortIdentity, PtpMessage, PtpTimestamp, decode,
    encode,
};
use domain::shared_kernel::TimeDelta;
use socket2::SockRef;

/// 疑似マスタの識別子（ローカルに割り当てたアドレス）
const IDENTITY: PortIdentity = PortIdentity {
    clock: [0x02, 0x00, 0x00, 0xFF, 0xFE, 0x00, 0x00, 0x01],
    port: 1,
};
/// TAI − UTC（秒）
const UTC_OFFSET: i16 = 37;
/// 送信のスレッドが停止の合図を確かめる間隔
const STOP_CHECK: Duration = Duration::from_millis(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimPtpMasterConfig {
    /// 待ち受けるイベント用のポート（一般用はその次の番号）
    pub event_port: u16,
    /// 送り先（イベント用のポート。一般用はその次の番号）。マルチキャストのグループかスレーブのアドレス
    pub destination: SocketAddr,
    /// マルチキャストで送るインターフェースのアドレス（`None` なら経路で決まる）
    pub interface: Option<Ipv4Addr>,
    pub domain: u8,
    /// PC の壁時計に対して、マスタの時刻をどれだけ進めるか（負なら遅らせる）
    pub offset: TimeDelta,
    pub two_step: bool,
    /// Sync の間隔の log2（秒）
    pub log_sync_interval: i8,
    /// Announce の間隔の log2（秒）。Announce は Sync と一緒にしか送らないので、Sync の間隔より短くしたときは
    /// Sync の間隔で送り、その間隔を知らせる（短い間隔を知らせると、スレーブは届かないとみなしてマスタに選ばない）
    pub log_announce_interval: i8,
}

/// 疑似 PTP マスタ。drop すると止まる。
#[derive(Debug)]
pub struct SimPtpMaster {
    stop: Arc<AtomicBool>,
    event_port: u16,
    /// 止めるときに、受信を待つスレッドを起こす宛先
    wake: SocketAddr,
    announcer: Option<JoinHandle<()>>,
    responder: Option<JoinHandle<()>>,
}

impl SimPtpMaster {
    /// 起動して、別スレッドで送信と応答を始める。
    ///
    /// # Errors
    ///
    /// ポートを使えない、またはスレッドを起動できないとき。
    pub fn spawn(config: SimPtpMasterConfig) -> io::Result<Self> {
        let bind_ip = bind_address(&config);
        let event = UdpSocket::bind((bind_ip, config.event_port))?;
        let general = UdpSocket::bind((bind_ip, config.event_port.saturating_add(1)))?;
        if let Some(group) = multicast_group(&config) {
            let interface = config.interface.unwrap_or(Ipv4Addr::UNSPECIFIED);
            for socket in [&event, &general] {
                socket.set_multicast_ttl_v4(1)?;
                if config.interface.is_some() {
                    SockRef::from(socket).set_multicast_if_v4(&interface)?;
                }
                // 参加していないグループ（ほかのプログラムが参加したもの）のマルチキャストは受け取らない
                #[cfg(target_os = "linux")]
                SockRef::from(socket).set_multicast_all_v4(false)?;
            }
            // 受け取るのはイベント用のポートの `Delay_Req` だけ
            event.join_multicast_v4(&group, &interface)?;
        }
        let event_port = event.local_addr()?.port();
        let wake = SocketAddr::from((
            if bind_ip.is_unspecified() {
                Ipv4Addr::LOCALHOST
            } else {
                bind_ip
            },
            event_port,
        ));
        let stop = Arc::new(AtomicBool::new(false));
        let announcer = {
            let announce_event = event.try_clone()?;
            let announce_general = general.try_clone()?;
            let announce_stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("sim-ptp-announce".to_owned())
                .spawn(move || {
                    announce_loop(&announce_event, &announce_general, config, &announce_stop);
                })?
        };
        let responder = {
            let respond_stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("sim-ptp-respond".to_owned())
                .spawn(move || respond_loop(&event, &general, config, &respond_stop))?
        };
        Ok(Self {
            stop,
            event_port,
            wake,
            announcer: Some(announcer),
            responder: Some(responder),
        })
    }

    /// 待ち受けているイベント用のポート。
    #[must_use]
    pub const fn event_port(&self) -> u16 {
        self.event_port
    }

    /// スレッドが終わるまで待つ（CLI で動かし続ける用）。
    pub fn join(mut self) {
        if let Some(handle) = self.announcer.take() {
            drop(handle.join());
        }
    }
}

impl Drop for SimPtpMaster {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.announcer.take() {
            drop(handle.join());
        }
        if let Some(handle) = self.responder.take() {
            // 受信を待っているので、空のデータグラムで起こす。起こせなかったら待たない
            let woken = UdpSocket::bind((self.wake.ip(), 0))
                .and_then(|socket| socket.send_to(&[], self.wake));
            if woken.is_ok() {
                drop(handle.join());
            }
        }
    }
}

/// 送り先がマルチキャストのグループなら、そのグループ。
const fn multicast_group(config: &SimPtpMasterConfig) -> Option<Ipv4Addr> {
    match config.destination.ip() {
        IpAddr::V4(group) if group.is_multicast() => Some(group),
        IpAddr::V4(_) | IpAddr::V6(_) => None,
    }
}

/// bind するアドレス。マルチキャストなら全アドレス、ユニキャストなら `interface`（なければ全アドレス）。
fn bind_address(config: &SimPtpMasterConfig) -> Ipv4Addr {
    if multicast_group(config).is_some() {
        Ipv4Addr::UNSPECIFIED
    } else {
        config.interface.unwrap_or(Ipv4Addr::UNSPECIFIED)
    }
}

/// いまのマスタの時刻（TAI）。
fn master_now(offset: TimeDelta) -> PtpTimestamp {
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let tai = i128::try_from(unix).unwrap_or(i128::MAX)
        + i128::from(offset.as_nanos())
        + i128::from(UTC_OFFSET) * 1_000_000_000;
    let nanos = u128::try_from(tai.max(0)).unwrap_or(0);
    PtpTimestamp {
        seconds: u64::try_from(nanos / 1_000_000_000).unwrap_or(u64::MAX),
        nanos: u32::try_from(nanos % 1_000_000_000).unwrap_or(0),
    }
}

const fn header(config: &SimPtpMasterConfig, flags: Flags, sequence_id: u16, log: i8) -> Header {
    Header {
        transport_specific: 0,
        version: Header::VERSION_2,
        domain: config.domain,
        minor_sdo_id: 0,
        flags,
        correction: Correction(0),
        source: IDENTITY,
        sequence_id,
        log_message_interval: log,
    }
}

/// 一般用のポート（イベント用 + 1）。
const fn general_of(address: SocketAddr) -> SocketAddr {
    SocketAddr::new(address.ip(), address.port().saturating_add(1))
}

/// Sync の間隔の時間。
fn sync_interval(log: i8) -> Duration {
    log_interval(log).to_std_saturating()
}

/// 送れなかった回は飛ばして続ける（スレーブの側で応答なしとして見える）。
fn announce_loop(
    event: &UdpSocket,
    general: &UdpSocket,
    config: SimPtpMasterConfig,
    stop: &AtomicBool,
) {
    let interval = sync_interval(config.log_sync_interval);
    // Announce は Sync と一緒に送るので、Sync より短い間隔では送れない。実際に送る間隔を知らせる
    let log_announce_interval = config.log_announce_interval.max(config.log_sync_interval);
    let announce_every = sync_interval(log_announce_interval);
    let mut sync_sequence = 0u16;
    let mut announce_sequence = 0u16;
    let mut since_announce = announce_every;
    while !stop.load(Ordering::Relaxed) {
        if since_announce >= announce_every {
            since_announce = Duration::ZERO;
            let announce = PtpMessage {
                header: header(
                    &config,
                    Flags::PTP_TIMESCALE.with(Flags::UTC_OFFSET_VALID),
                    announce_sequence,
                    log_announce_interval,
                ),
                // clockClass 248（既定）・精度不明・時刻源は内部の発振器（0xA0）
                body: Body::Announce(Announce {
                    origin: master_now(config.offset),
                    current_utc_offset: UTC_OFFSET,
                    grandmaster_priority1: 128,
                    grandmaster_class: 248,
                    grandmaster_accuracy: 0xFE,
                    grandmaster_variance: 0xFFFF,
                    grandmaster_priority2: 128,
                    grandmaster_identity: IDENTITY.clock,
                    steps_removed: 0,
                    time_source: 0xA0,
                }),
            };
            drop(general.send_to(&encode(&announce), general_of(config.destination)));
            announce_sequence = announce_sequence.wrapping_add(1);
        }
        let flags = if config.two_step {
            Flags::TWO_STEP
        } else {
            Flags(0)
        };
        let origin = master_now(config.offset);
        let sync = PtpMessage {
            header: header(&config, flags, sync_sequence, config.log_sync_interval),
            body: Body::Sync { origin },
        };
        drop(event.send_to(&encode(&sync), config.destination));
        if config.two_step {
            let follow_up = PtpMessage {
                header: header(&config, Flags(0), sync_sequence, config.log_sync_interval),
                body: Body::FollowUp {
                    precise_origin: origin,
                },
            };
            drop(general.send_to(&encode(&follow_up), general_of(config.destination)));
        }
        sync_sequence = sync_sequence.wrapping_add(1);
        // 停止の合図を確かめながら、次の Sync まで待つ
        let mut waited = Duration::ZERO;
        while waited < interval && !stop.load(Ordering::Relaxed) {
            let slice = STOP_CHECK.min(interval.saturating_sub(waited));
            thread::sleep(slice);
            waited += slice;
        }
        since_announce += interval;
    }
}

fn respond_loop(
    event: &UdpSocket,
    general: &UdpSocket,
    config: SimPtpMasterConfig,
    stop: &AtomicBool,
) {
    let mut buf = [0u8; 1500];
    while !stop.load(Ordering::Relaxed) {
        // 相手が消えたあとの ConnectionReset（Windows）は無視して続ける。止めるときの空のデータグラムは
        // 解析できないので読み捨て、ループの先頭で停止の合図を見る
        let Ok((len, peer)) = event.recv_from(&mut buf) else {
            continue;
        };
        let receive = master_now(config.offset);
        let Ok(request) = decode(&buf[..len]) else {
            continue;
        };
        if !matches!(request.body, Body::DelayReq { .. }) || request.header.domain != config.domain
        {
            continue;
        }
        let unicast = request.header.flags.contains(Flags::UNICAST);
        let (flags, log, to) = if unicast {
            (
                Flags::UNICAST,
                Header::LOG_INTERVAL_UNSPECIFIED,
                general_of(peer),
            )
        } else {
            (Flags(0), 0, general_of(config.destination))
        };
        let response = PtpMessage {
            header: header(&config, flags, request.header.sequence_id, log),
            body: Body::DelayResp {
                receive,
                requesting: request.header.source,
            },
        };
        drop(general.send_to(&encode(&response), to));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(destination: &str, interface: Option<Ipv4Addr>) -> SimPtpMasterConfig {
        SimPtpMasterConfig {
            event_port: 12319,
            destination: destination.parse().unwrap(),
            interface,
            domain: 0,
            offset: TimeDelta::ZERO,
            two_step: true,
            log_sync_interval: 0,
            log_announce_interval: 0,
        }
    }

    #[test]
    fn multicast_binds_to_all_addresses_to_receive_delay_requests() {
        let lan = Some(Ipv4Addr::new(192, 168, 10, 200));
        let multicast = config("224.0.1.129:319", lan);
        assert_eq!(
            multicast_group(&multicast),
            Some(Ipv4Addr::new(224, 0, 1, 129))
        );
        // Linux では、特定のアドレスに bind するとマルチキャストの Delay_Req が届かない
        assert_eq!(bind_address(&multicast), Ipv4Addr::UNSPECIFIED);

        let unicast = config("127.0.0.1:13319", Some(Ipv4Addr::LOCALHOST));
        assert_eq!(multicast_group(&unicast), None);
        assert_eq!(bind_address(&unicast), Ipv4Addr::LOCALHOST);
        assert_eq!(
            bind_address(&config("192.168.10.4:319", None)),
            Ipv4Addr::UNSPECIFIED
        );
    }
}
