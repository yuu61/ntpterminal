//! 組み立て役（composition root）。各層の部品をつなぐだけで、ロジックは持たない。
//!
//! オプションは `Option` で受け取り、設定ファイルの値で埋めてから（`config`）、残りを既定値にする。
#![forbid(unsafe_code)]

extern crate alloc;

mod config;

use alloc::sync::Arc;
use core::fmt;
use core::net::{IpAddr, Ipv4Addr, SocketAddr};
use core::str::FromStr;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{self, ExitCode};
use std::sync::mpsc;
use std::thread;

use anyhow::Context as _;
use application::acl::mib::MibProfile;
use application::polling_policy;
use application::port::{ClockPort, PacketPort, PhcPort};
use application::snapshot::ReceiverSnapshot;
use application::timestamping::TimestampingStatus;
use application::usecase::{
    PhcRequest, PhcSelectionError, PhcService, Ptp4lClient, Ptp4lClientConfig, ReceiverMonitor,
    ReceiverMonitorConfig, SyncService, query_once, select_phc,
};
use clap::{Args, Parser, Subcommand, ValueEnum};
use domain::packet_crafting::{
    EthernetSpec, HeaderTemplate, Ipv4Spec, MacAddr, RequestTemplate, UdpChecksum, VlanTag,
};
use domain::ptp_clock::PhcConfig;
use domain::shared_kernel::TimeDelta;
use domain::time_sync::SourceConfig;
use infrastructure::clock::SystemClock;
use infrastructure::nonce::RandomNonce;
use infrastructure::ptp4l;
use infrastructure::resolve::resolve_server;
use infrastructure::sim_server::{SimServer, SimServerConfig, SimSnmpAgent};
use infrastructure::transport::{
    self as transport_layer, LinkOptions, OpenedTransport, TimestampingMode, TransportMode,
    TransportOptions, UdpTransport,
};
use presentation::cli::once;
use presentation::cli::phc::{self, PhcStatsReporter, PhcTarget};
use presentation::cli::stats::StatsReporter;
use presentation::format::{DisplayZone, timestamping_label};
use presentation::tui::{
    self, PhcSnapshotSender, SnapshotSender, SourceSnapshot, TuiConfig, TuiSource,
};
use serde::Deserialize;

use crate::config::ConfigFile;

/// `--config` を省略したときに読む設定ファイル（カレントディレクトリ。なければ読まない）
const DEFAULT_CONFIG_PATH: &str = "config.toml";

// ---- 既定値（コマンドラインにも設定ファイルにもないときに使う。ヘルプの「既定:」や docs/configuration.md の表と揃える） ----
const DEFAULT_INTERVAL_MS: u64 = 500;
/// ポーリング間隔の下限（ミリ秒）。これより短い指定はこの値にする
const MIN_INTERVAL_MS: u64 = 10;
const DEFAULT_TITLE: &str = "NTP CLOCK";
const DEFAULT_TIMEOUT_MS: u64 = 1000;
const DEFAULT_COUNT: u64 = 20;
const DEFAULT_SIM_BIND: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12300);
const DEFAULT_SIM_OFFSET_MS: f64 = 0.0;
const DEFAULT_SIM_STRATUM: u8 = 1;
const DEFAULT_SIM_REFID: &str = "SIM";
const DEFAULT_SNMP_PORT: u16 = 161;
const DEFAULT_SNMP_INTERVAL_MS: u64 = 5000;
/// 受信機を読む間隔の下限（ミリ秒）。これより短い指定はこの値にする
const MIN_SNMP_INTERVAL_MS: u64 = 1000;
/// SNMP の GET で応答を待つ上限（ミリ秒）
const SNMP_TIMEOUT_MS: u64 = 1000;
const DEFAULT_SIM_SNMP_BIND: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 16100);
/// `--ptp4l` のときの見出し
const DEFAULT_PTP_TITLE: &str = "PTP CLOCK";
const DEFAULT_PTP_DOMAIN: u8 = 0;
/// tui / stats で ptp4l の応答を待つ上限（ミリ秒）。ptp4l は同じホストなので、ふつうは 1 ms もかからない
const PTP4L_TIMEOUT_MS: u64 = 200;

// ---- 値の範囲（コマンドラインと設定ファイルの両方で確かめる） ----
/// DSCP（6 bit）
const DSCP_MAX: u8 = 63;
/// ECN（2 bit）
const ECN_MAX: u8 = 3;
/// 802.1Q の VLAN ID（12 bit。4095 は予約）
const VLAN_ID_MAX: u16 = 4094;
/// 802.1Q の優先度 PCP（3 bit）
const PCP_MAX: u8 = 7;

#[derive(Parser)]
#[command(
    name = "ntpterminal",
    version,
    about = "GNSS タイムサーバのデモ用 NTP クロック"
)]
struct Cli {
    /// 設定ファイル（TOML。省略時はカレントディレクトリの config.toml があれば読む）。キーは長いオプションと同じ名前で、コマンドラインの指定が優先される（例: docs/config.example.toml）
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 大きい時計の TUI を表示する（q / Esc / Ctrl+C で終了）
    Tui(TuiArgs),
    /// 1 回だけ問い合わせて結果を表示する
    Once(OnceArgs),
    /// 一定間隔で問い合わせ続け、offset と delay の統計を表示する
    Stats(StatsArgs),
    /// 開発用: 疑似 NTP サーバを起動する（実機がなくても試せる）
    SimServer(SimServerArgs),
}

#[derive(Args)]
struct TargetArgs {
    /// NTP サーバ（例: 192.168.1.123 / ntp.example.com / 127.0.0.1:12300）。ポート省略時は 123。省略時は設定ファイルの server
    server: Option<String>,
    /// 結果を JSON で出力する
    #[arg(long)]
    json: bool,
}

/// ptp4l が合わせる PHC を表示する指定（tui / once / stats）。
#[derive(Args, Clone)]
struct PtpArgs {
    /// NTP サーバの代わりに、ptp4l（linuxptp）が合わせる NIC の時計（PHC）の時刻を表示する（Linux のみ・root が必要）
    #[arg(long)]
    ptp4l: bool,
    /// --ptp4l のみ: ptp4l の管理用ソケット（既定: /var/run/ptp/ptp4l、なければ /var/run/ptp4l）
    #[arg(long, value_name = "PATH")]
    ptp4l_socket: Option<PathBuf>,
    /// --ptp4l のみ: ptp4l のドメイン番号（既定: 0）
    #[arg(long)]
    ptp_domain: Option<u8>,
    /// --ptp4l のみ: 表示する PHC（/dev/ptpN か、ptp4l のポートのインターフェース名）。ptp4l が複数の PHC を使うときに指定する
    #[arg(long, value_name = "PHC", value_parser = parse_phc)]
    phc: Option<PhcRequest>,
}

impl PtpArgs {
    /// 一緒に使えない指定をエラーにする（黙って無視しない）。`--ptp4l` のときは NTP サーバと `ntp_only`
    /// （NTP サーバに問い合わせるときだけの指定。名前と、指定したか）を、そうでないときは `--ptp4l` だけの指定を確かめる。
    fn check(&self, server: Option<&str>, ntp_only: &[(&'static str, bool)]) -> anyhow::Result<()> {
        if self.ptp4l {
            anyhow::ensure!(
                server.is_none(),
                "--ptp4l と NTP サーバ（引数か、設定ファイルの server）は一緒に指定できません"
            );
            if let Some(&(name, _)) = ntp_only.iter().find(|&&(_, given)| given) {
                anyhow::bail!(
                    "{name} は NTP サーバに問い合わせるときの指定で、--ptp4l とは一緒に使えません（設定ファイルの値も含む）"
                );
            }
            return Ok(());
        }
        let ptp_only = [
            ("--ptp4l-socket", self.ptp4l_socket.is_some()),
            ("--ptp-domain", self.ptp_domain.is_some()),
            ("--phc", self.phc.is_some()),
        ];
        if let Some(&(name, _)) = ptp_only.iter().find(|&&(_, given)| given) {
            anyhow::bail!("{name} は --ptp4l と一緒に指定してください");
        }
        Ok(())
    }
}

/// `--phc` の値: `/dev/ptpN` なら PHC の番号、それ以外は ptp4l のポートのインターフェース名。
fn parse_phc(text: &str) -> Result<PhcRequest, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("PHC は /dev/ptpN かインターフェース名で指定してください".to_owned());
    }
    // 数字だけはインターフェース名とせず、/dev/ptpN の書き間違いとして知らせる
    if trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "PHC は /dev/ptp{trimmed} の形で指定してください（数字だけはインターフェース名とみなしません）"
        ));
    }
    trimmed.strip_prefix("/dev/ptp").map_or_else(
        || Ok(PhcRequest::Interface(trimmed.to_owned())),
        |number| {
            number
                .parse::<u32>()
                .map(PhcRequest::Index)
                .map_err(|error| {
                    format!("PHC は /dev/ptp0 の形で指定してください: {trimmed}（{error}）")
                })
        },
    )
}

#[derive(Args)]
struct ZoneArgs {
    /// 表示用のタイムゾーン（UTC からのずれ。既定: +09:00）
    #[arg(long, allow_hyphen_values = true, value_parser = DisplayZone::parse_offset)]
    tz: Option<i32>,
    /// タイムゾーンの表示名（既定: JST）
    #[arg(long)]
    tz_label: Option<String>,
}

/// 設定ファイルでの綴りは、コマンドラインの値と同じ（`raw-ip` など）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ModeArg {
    /// 標準の UDP ソケット（権限不要・全 OS）
    #[default]
    Udp,
    /// IPv4 / UDP ヘッダを自作して raw ソケットで送る（Linux のみ・root か `CAP_NET_RAW` が必要）
    RawIp,
    /// Ethernet フレームから自作して `AF_PACKET` で送る（Linux のみ・root か `CAP_NET_RAW` が必要）
    Link,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum TimestampingArg {
    /// 使えればカーネル、使えなければユーザ空間
    #[default]
    Auto,
    /// カーネルのタイムスタンプ（Linux のみ。使えなければエラー）
    Kernel,
    /// ユーザ空間で時計を読む
    User,
    /// NIC のハードウェアタイムスタンプ（Linux のみ・root が必要。NIC で無効なら有効にして、終了時に戻す。使えなければエラー）
    Hardware,
}

#[derive(Args, Clone, Copy)]
struct TransportArgs {
    /// 送信方式（既定: udp）
    #[arg(long = "transport", value_enum)]
    mode: Option<ModeArg>,
    /// タイムスタンプの取り方（既定: auto）
    #[arg(long, value_enum)]
    timestamping: Option<TimestampingArg>,
    /// 送信元ポート（省略時は OS に任せる）
    #[arg(long)]
    source_port: Option<u16>,
    /// raw-ip / link のみ: IP の TTL
    #[arg(long)]
    ttl: Option<u8>,
    /// raw-ip / link のみ: DSCP（0〜63。例: 46 = EF）
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=i64::from(DSCP_MAX)))]
    dscp: Option<u8>,
    /// raw-ip / link のみ: ECN（0〜3）
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=i64::from(ECN_MAX)))]
    ecn: Option<u8>,
    /// raw-ip / link のみ: IP の Identification（raw-ip では 0 にするとカーネルが値を入れる）
    #[arg(long)]
    ip_id: Option<u16>,
    /// raw-ip / link のみ: DF（Don't Fragment）ビットを立てない
    #[arg(long)]
    no_df: bool,
    /// raw-ip / link のみ: UDP チェックサム: compute（既定）/ zero / 0x で始まる 16 進の固定値（試験用）
    #[arg(long, value_parser = parse_udp_checksum)]
    udp_checksum: Option<UdpChecksum>,
    /// link のみ: IP ヘッダのチェックサム（0x で始まる 16 進の固定値。試験用）
    #[arg(long, value_parser = parse_hex_u16)]
    ip_checksum: Option<u16>,
    /// link のみ: 宛先 MAC（省略時は近隣表か ARP で調べる）
    #[arg(long)]
    dst_mac: Option<MacAddr>,
    /// link のみ: 802.1Q の VLAN ID（0〜4094）
    #[arg(long, value_parser = clap::value_parser!(u16).range(0..=i64::from(VLAN_ID_MAX)))]
    vlan: Option<u16>,
    /// link のみ: 802.1Q の優先度 PCP（0〜7。--vlan なしなら VLAN ID 0 の優先度タグになる）
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=i64::from(PCP_MAX)))]
    pcp: Option<u8>,
}

impl TransportArgs {
    /// 送受信の項目と、それを指定したか（`--ptp4l` と一緒に使えないものを知らせるため）。
    fn given(&self) -> Vec<(&'static str, bool)> {
        vec![
            ("--transport", self.mode.is_some()),
            ("--timestamping", self.timestamping.is_some()),
            ("--source-port", self.source_port.is_some()),
            ("--ttl", self.ttl.is_some()),
            ("--dscp", self.dscp.is_some()),
            ("--ecn", self.ecn.is_some()),
            ("--ip-id", self.ip_id.is_some()),
            ("--no-df", self.no_df),
            ("--udp-checksum", self.udp_checksum.is_some()),
            ("--ip-checksum", self.ip_checksum.is_some()),
            ("--dst-mac", self.dst_mac.is_some()),
            ("--vlan", self.vlan.is_some()),
            ("--pcp", self.pcp.is_some()),
        ]
    }

    fn mode(&self) -> TransportMode {
        match self.mode.unwrap_or_default() {
            ModeArg::Udp => TransportMode::Udp,
            ModeArg::RawIp => TransportMode::RawIp,
            ModeArg::Link => TransportMode::Link,
        }
    }

    /// ヘッダの指定が 1 つでもあれば `headers` / `link` を作る（使えないモードでは open がエラーにする）。
    fn options(&self) -> TransportOptions {
        let specified = self.ttl.is_some()
            || self.dscp.is_some()
            || self.ecn.is_some()
            || self.ip_id.is_some()
            || self.no_df
            || self.udp_checksum.is_some()
            || self.ip_checksum.is_some();
        let defaults = Ipv4Spec::default();
        let headers = specified.then(|| HeaderTemplate {
            ipv4: Ipv4Spec {
                dscp: self.dscp.unwrap_or(defaults.dscp),
                ecn: self.ecn.unwrap_or(defaults.ecn),
                identification: self.ip_id.unwrap_or(defaults.identification),
                dont_fragment: !self.no_df,
                ttl: self.ttl.unwrap_or(defaults.ttl),
                header_checksum: self.ip_checksum,
            },
            udp_checksum: self.udp_checksum.unwrap_or_default(),
        });
        let vlan = (self.vlan.is_some() || self.pcp.is_some()).then(|| VlanTag {
            pcp: self.pcp.unwrap_or(0),
            dei: false,
            vid: self.vlan.unwrap_or(0),
        });
        let link = (self.dst_mac.is_some() || vlan.is_some()).then_some(LinkOptions {
            destination_mac: self.dst_mac,
            ethernet: EthernetSpec { vlan },
        });
        TransportOptions {
            source_port: self.source_port.unwrap_or(0),
            headers,
            link,
            timestamping: match self.timestamping.unwrap_or_default() {
                TimestampingArg::Auto => TimestampingMode::Auto,
                TimestampingArg::Kernel => TimestampingMode::Kernel,
                TimestampingArg::User => TimestampingMode::User,
                TimestampingArg::Hardware => TimestampingMode::Hardware,
            },
        }
    }

    fn open(
        &self,
        server: SocketAddr,
        clock: Arc<dyn ClockPort>,
        status: Arc<TimestampingStatus>,
    ) -> anyhow::Result<OpenedTransport> {
        transport_layer::open(self.mode(), server, self.options(), clock, status)
            .context("送受信の準備ができません")
    }
}

fn parse_hex_u16(text: &str) -> Result<u16, String> {
    text.strip_prefix("0x")
        .and_then(|hex| u16::from_str_radix(hex, 16).ok())
        .ok_or_else(|| "0x1234 の形式で指定してください".to_owned())
}

fn parse_udp_checksum(text: &str) -> Result<UdpChecksum, String> {
    match text {
        "compute" => Ok(UdpChecksum::Compute),
        "zero" => Ok(UdpChecksum::Zero),
        _ => text
            .strip_prefix("0x")
            .and_then(|hex| u16::from_str_radix(hex, 16).ok())
            .map(UdpChecksum::Fixed)
            .ok_or_else(|| "compute / zero / 0x1234 の形式で指定してください".to_owned()),
    }
}

/// 読む MIB。設定ファイルでの綴りは、コマンドラインの値と同じ（`meinberg-ltng` など）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum MibArg {
    /// 応答する MIB を探す（Meinberg は新しい MIB から試す）
    #[default]
    Auto,
    /// Meinberg MEINBERG-OS-MIB（meinbergOS の機器）
    MeinbergOs,
    /// Meinberg MBG-SNMP-LTNG-MIB（LANTIME の現行ファームウェア）
    MeinbergLtng,
    /// Meinberg MBG-SNMP-LT-MIB（旧 LANTIME の V5 系）
    MeinbergLt,
    /// Microchip SyncServer S650
    MicrochipS650,
}

impl MibArg {
    const fn profile(self) -> Option<MibProfile> {
        match self {
            Self::Auto => None,
            Self::MeinbergOs => Some(MibProfile::MeinbergOs),
            Self::MeinbergLtng => Some(MibProfile::MeinbergLtng),
            Self::MeinbergLt => Some(MibProfile::MeinbergLt),
            Self::MicrochipS650 => Some(MibProfile::MicrochipS650),
        }
    }
}

/// 疑似 SNMP エージェントが返す MIB（`auto` はない）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum SimMibArg {
    /// Meinberg MEINBERG-OS-MIB
    MeinbergOs,
    /// Meinberg MBG-SNMP-LTNG-MIB
    MeinbergLtng,
    /// Meinberg MBG-SNMP-LT-MIB
    MeinbergLt,
    /// Microchip SyncServer S650
    MicrochipS650,
}

impl SimMibArg {
    const fn profile(self) -> MibProfile {
        match self {
            Self::MeinbergOs => MibProfile::MeinbergOs,
            Self::MeinbergLtng => MibProfile::MeinbergLtng,
            Self::MeinbergLt => MibProfile::MeinbergLt,
            Self::MicrochipS650 => MibProfile::MicrochipS650,
        }
    }
}

/// GNSS 受信機の状態を SNMP v2c で読む指定（tui）。
#[derive(Args, Clone)]
struct SnmpArgs {
    /// GNSS 受信機の状態を SNMP v2c で読むときのコミュニティ名。指定すると、NTP サーバと同じホストに問い合わせて受信機の行を出す（画面には出さない）
    #[arg(
        id = "snmp_community",
        long = "snmp-community",
        value_name = "COMMUNITY"
    )]
    community: Option<String>,
    /// SNMP のポート（既定: 161。--snmp-community と一緒に指定する）
    #[arg(id = "snmp_port", long = "snmp-port")]
    port: Option<u16>,
    /// 受信機を読む間隔（ミリ秒。既定: 5000、下限: 1000。--snmp-community と一緒に指定する）
    #[arg(id = "snmp_interval_ms", long = "snmp-interval-ms")]
    interval_ms: Option<u64>,
    /// 読む MIB（既定: auto。--snmp-community と一緒に指定する）
    #[arg(id = "snmp_mib", long = "snmp-mib", value_enum)]
    mib: Option<MibArg>,
}

impl SnmpArgs {
    /// SNMP の項目と、それを指定したか（`--ptp4l` と一緒に使えないものを知らせるため）。
    const fn given(&self) -> [(&'static str, bool); 4] {
        [
            ("--snmp-community", self.community.is_some()),
            ("--snmp-port", self.port.is_some()),
            ("--snmp-interval-ms", self.interval_ms.is_some()),
            ("--snmp-mib", self.mib.is_some()),
        ]
    }

    /// 受信機を読む宛先と設定。`--snmp-community` がなければ `None`（読まない）。
    ///
    /// # Errors
    ///
    /// `--snmp-community` なしで、ほかの SNMP の指定があるとき（黙って無視しない）。
    fn monitor(
        &self,
        server: SocketAddr,
    ) -> anyhow::Result<Option<(SocketAddr, ReceiverMonitorConfig)>> {
        let Some(community) = self.community.as_deref() else {
            anyhow::ensure!(
                self.port.is_none() && self.interval_ms.is_none() && self.mib.is_none(),
                "--snmp-port・--snmp-interval-ms・--snmp-mib は --snmp-community と一緒に指定してください"
            );
            return Ok(None);
        };
        let target = SocketAddr::new(server.ip(), self.port.unwrap_or(DEFAULT_SNMP_PORT));
        let config = ReceiverMonitorConfig {
            community: community.as_bytes().to_vec(),
            profile: self.mib.unwrap_or_default().profile(),
            interval: Duration::from_millis(
                self.interval_ms
                    .unwrap_or(DEFAULT_SNMP_INTERVAL_MS)
                    .max(MIN_SNMP_INTERVAL_MS),
            ),
            timeout: Duration::from_millis(SNMP_TIMEOUT_MS),
        };
        Ok(Some((target, config)))
    }
}

/// NTP の Reference ID（4 文字以内の ASCII）。
#[derive(Clone, Debug, PartialEq, Eq)]
struct RefId(String);

impl RefId {
    fn bytes(&self) -> [u8; 4] {
        let mut bytes = [0u8; 4];
        bytes[..self.0.len()].copy_from_slice(self.0.as_bytes());
        bytes
    }
}

impl FromStr for RefId {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.is_ascii() && text.len() <= 4 {
            Ok(Self(text.to_owned()))
        } else {
            Err(format!(
                "Reference ID は 4 文字以内の ASCII で指定してください: {text}"
            ))
        }
    }
}

impl fmt::Display for RefId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl ZoneArgs {
    fn zone(&self) -> DisplayZone {
        let default = DisplayZone::jst();
        DisplayZone::new(
            self.tz.unwrap_or_else(|| default.offset_secs()),
            self.tz_label
                .clone()
                .unwrap_or_else(|| default.label().to_owned()),
        )
    }
}

#[derive(Args)]
struct TuiArgs {
    /// NTP サーバ（例: 192.168.1.123 / 127.0.0.1:12300）。ポート省略時は 123。省略時は設定ファイルの server
    server: Option<String>,
    /// ポーリング間隔（ミリ秒。既定: 500）
    #[arg(long)]
    interval_ms: Option<u64>,
    /// 公開サーバへの短い間隔のポーリングを許可する（自分で管理するサーバに限ること。設定ファイルには書けない）
    #[arg(long)]
    allow_fast_poll: bool,
    /// 最上部の見出し（既定: NTP CLOCK）
    #[arg(long)]
    title: Option<String>,
    /// 最下部に出す一文（スポンサー表記など）
    #[arg(long)]
    footer: Option<String>,
    /// 直近の測定値の推移（オフセットと遅延の棒グラフ）を出す。時計が縮まない大きさの端末でだけ出る
    #[arg(long)]
    trend: bool,
    #[command(flatten)]
    snmp: SnmpArgs,
    #[command(flatten)]
    zone: ZoneArgs,
    #[command(flatten)]
    transport: TransportArgs,
    #[command(flatten)]
    ptp: PtpArgs,
}

#[derive(Args)]
struct OnceArgs {
    #[command(flatten)]
    target: TargetArgs,
    /// 応答を待つ上限（ミリ秒。既定: 1000）
    #[arg(long)]
    timeout_ms: Option<u64>,
    #[command(flatten)]
    zone: ZoneArgs,
    #[command(flatten)]
    transport: TransportArgs,
    #[command(flatten)]
    ptp: PtpArgs,
}

#[derive(Args)]
struct StatsArgs {
    #[command(flatten)]
    target: TargetArgs,
    /// ポーリング間隔（ミリ秒。既定: 500）
    #[arg(long)]
    interval_ms: Option<u64>,
    /// 試行回数（0 = 無制限。Ctrl+C で終了。既定: 20）
    #[arg(long)]
    count: Option<u64>,
    /// 公開サーバへの短い間隔のポーリングを許可する（自分で管理するサーバに限ること。設定ファイルには書けない）
    #[arg(long)]
    allow_fast_poll: bool,
    #[command(flatten)]
    transport: TransportArgs,
    #[command(flatten)]
    ptp: PtpArgs,
}

#[derive(Args)]
struct SimServerArgs {
    /// 待ち受けるアドレス（既定: 127.0.0.1:12300）
    #[arg(long)]
    bind: Option<SocketAddr>,
    /// PC の時計に対するサーバ時刻のずれ（ミリ秒。負も可。既定: 0）
    #[arg(long, allow_hyphen_values = true)]
    offset_ms: Option<f64>,
    /// 応答に入れる stratum（既定: 1）
    #[arg(long)]
    stratum: Option<u8>,
    /// 応答に入れる Reference ID（4 文字以内の ASCII。既定: SIM）
    #[arg(long)]
    refid: Option<RefId>,
    /// 疑似 SNMP エージェントも起動し、この MIB の GNSS 受信機の値を返す（コミュニティ名は public）
    #[arg(long, value_enum)]
    snmp_mib: Option<SimMibArg>,
    /// 疑似 SNMP エージェントの待ち受けアドレス（既定: 127.0.0.1:16100。--snmp-mib と一緒に指定する）
    #[arg(long)]
    snmp_bind: Option<SocketAddr>,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(error) => {
            // 標準エラーにも書けないときは、終了コードだけで知らせる
            drop(writeln!(io::stderr().lock(), "エラー: {error:#}"));
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let file = match cli.config.as_deref() {
        Some(path) => load_config(path)?,
        None => load_config_if_exists(Path::new(DEFAULT_CONFIG_PATH))?,
    };
    match file.fill(cli.command) {
        Command::Tui(args) => run_tui(&args),
        Command::Once(args) => run_once(&args),
        Command::Stats(args) => run_stats(&args),
        Command::SimServer(args) => run_sim_server(&args),
    }
}

fn load_config(path: &Path) -> anyhow::Result<ConfigFile> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("設定ファイルを読めません: {}", path.display()))?;
    config::parse(&text)
        .with_context(|| format!("設定ファイルの内容が正しくありません: {}", path.display()))
}

/// ファイルがなければ、設定ファイルなし（コマンドラインと既定値だけ）として扱う。
/// あるのに読めないとき・内容が正しくないときはエラーにする。
fn load_config_if_exists(path: &Path) -> anyhow::Result<ConfigFile> {
    if fs::metadata(path).is_err_and(|error| error.kind() == io::ErrorKind::NotFound) {
        return Ok(ConfigFile::default());
    }
    load_config(path)
}

fn run_tui(args: &TuiArgs) -> anyhow::Result<ExitCode> {
    let mut ntp_only = args.transport.given();
    ntp_only.extend(args.snmp.given());
    ntp_only.push(("--allow-fast-poll", args.allow_fast_poll));
    args.ptp.check(args.server.as_deref(), &ntp_only)?;
    let interval = polling_interval(args.interval_ms);
    // 同期スレッド・受信機の監視スレッドと UI は必ず同じ時計インスタンスを使う。
    // モノトニック時計の起点（作成時の Instant）が違うと、基準点からの計算が黙って狂う
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
    // 同期は別スレッドで回し、スナップショットだけをチャネルで UI に渡す（集約は共有しない）
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let started = if args.ptp.ptp4l {
        start_phc(args, &clock, interval, tx, &stop)?
    } else {
        start_ntp(args, &clock, interval, tx, &stop)?
    };
    let config = TuiConfig {
        title: args
            .title
            .clone()
            .unwrap_or_else(|| started.default_title.to_owned()),
        server_label: started.label,
        footer: args.footer.clone(),
        trend: args.trend,
        zone: args.zone.zone(),
        source: started.source,
    };
    let result = tui::run(
        &config,
        clock.as_ref(),
        &rx,
        started.receiver.as_ref().map(|monitor| &monitor.0),
    );

    stop.store(true, Ordering::Relaxed);
    drop(started.sync_thread.join());
    if let Some((_, monitor_thread)) = started.receiver {
        drop(monitor_thread.join());
    }
    result.context("端末の表示でエラーが起きました")?;
    Ok(ExitCode::SUCCESS)
}

/// TUI に渡す時刻源を始めた結果（同期スレッドと、受信機の監視）。
struct StartedSource {
    source: TuiSource,
    /// 見出しの下に出す名前
    label: String,
    /// `--title` がないときの見出し
    default_title: &'static str,
    sync_thread: thread::JoinHandle<()>,
    receiver: Option<(mpsc::Receiver<ReceiverSnapshot>, thread::JoinHandle<()>)>,
}

/// NTP サーバへの問い合わせ（と、指定があれば受信機の監視）を別スレッドで始める。
fn start_ntp(
    args: &TuiArgs,
    clock: &Arc<dyn ClockPort>,
    interval: Duration,
    tx: mpsc::Sender<SourceSnapshot>,
    stop: &Arc<AtomicBool>,
) -> anyhow::Result<StartedSource> {
    let server_text = required_server(args.server.as_deref())?;
    let server = resolve(server_text)?;
    if !args.allow_fast_poll {
        polling_policy::check_interval(server.ip(), interval)?;
    }
    let receiver = args.snmp.monitor(server)?;
    let timestamping = Arc::new(TimestampingStatus::new());
    let OpenedTransport {
        port: transport,
        description: transport_label,
    } = args
        .transport
        .open(server, Arc::clone(clock), Arc::clone(&timestamping))?;
    let mut service = SyncService::new(
        SourceConfig::with_poll_interval(TimeDelta::from_std(interval)),
        transport,
        RandomNonce::new(),
        Arc::clone(clock),
        RequestTemplate::default(),
    );
    let sync_thread = thread::Builder::new()
        .name("ntp-sync".into())
        .spawn({
            let thread_stop = Arc::clone(stop);
            move || service.run_until(&mut SnapshotSender::new(tx), None, &thread_stop)
        })
        .context("同期スレッドを起動できません")?;
    let receiver_monitor = match receiver {
        Some((target, monitor_config)) => Some(spawn_receiver_monitor(
            target,
            monitor_config,
            Arc::clone(clock),
            stop,
        )?),
        None => None,
    };
    Ok(StartedSource {
        source: TuiSource::Ntp {
            receiver: receiver_monitor.is_some(),
            transport: transport_label,
            timestamping,
        },
        label: server_text.trim().to_owned(),
        default_title: DEFAULT_TITLE,
        sync_thread,
        receiver: receiver_monitor,
    })
}

/// ptp4l が合わせる PHC の読み取りを別スレッドで始める。
fn start_phc(
    args: &TuiArgs,
    clock: &Arc<dyn ClockPort>,
    interval: Duration,
    tx: mpsc::Sender<SourceSnapshot>,
    stop: &Arc<AtomicBool>,
) -> anyhow::Result<StartedSource> {
    let (mut service, target) = open_ptp4l(
        &args.ptp,
        clock,
        interval,
        Duration::from_millis(PTP4L_TIMEOUT_MS),
    )?;
    let sync_thread = thread::Builder::new()
        .name("ptp4l-phc".into())
        .spawn({
            let thread_stop = Arc::clone(stop);
            move || service.run_until(&mut PhcSnapshotSender::new(tx), None, &thread_stop)
        })
        .context("同期スレッドを起動できません")?;
    Ok(StartedSource {
        label: target.ptp4l_label(),
        source: TuiSource::Phc {
            phc: target.phc,
            ports: target.ports,
        },
        default_title: DEFAULT_PTP_TITLE,
        sync_thread,
        receiver: None,
    })
}

/// ptp4l が合わせる PHC の同期サービス（PHC と ptp4l の管理用ソケットは、どちらも実行時に開く）。
type Ptp4lPhcService = PhcService<Box<dyn PhcPort>, Box<dyn PacketPort>>;

/// ptp4l につなぎ、ポートの一覧から表示する PHC を決めて開く。
fn open_ptp4l(
    ptp: &PtpArgs,
    clock: &Arc<dyn ClockPort>,
    interval: Duration,
    timeout: Duration,
) -> anyhow::Result<(Ptp4lPhcService, PhcTarget)> {
    let connection = ptp4l::connect(ptp.ptp4l_socket.as_deref(), Arc::clone(clock))
        .context("ptp4l につなげません（ソケットの場所は --ptp4l-socket で指定できます）")?;
    let domain = ptp.ptp_domain.unwrap_or(DEFAULT_PTP_DOMAIN);
    let mut client = Ptp4lClient::new(
        connection.port,
        Arc::clone(clock),
        Ptp4lClientConfig {
            domain,
            timeout,
            client_id: process::id(),
        },
    );
    let ports = client
        .discover_ports()
        .context(
            "ptp4l のポートを読めません（ドメイン番号は --ptp-domain、ソケットは --ptp4l-socket で指定できます）",
        )?;
    let selected = select_phc(&ports, ptp.phc.as_ref(), ptp4l::interface_phc).map_err(|error| {
        // --phc で選べるのは、ptp4l が複数の PHC を合わせているときだけ
        let context = if matches!(error, PhcSelectionError::Ambiguous { .. }) {
            "表示する PHC を決められません（--phc で指定できます）"
        } else {
            "表示する PHC を決められません"
        };
        anyhow::Error::new(error).context(context)
    })?;
    let opened = ptp4l::open_phc(selected.index, Arc::clone(clock)).context("PHC を開けません")?;
    let mut notes = Vec::new();
    if selected.vclock {
        notes.push("仮想 PHC");
    }
    if selected.from_interface {
        notes.push("インターフェースから特定");
    }
    notes.push(opened.method);
    let target = PhcTarget {
        socket: connection.path.display().to_string(),
        domain,
        phc: format!("{}（{}）", opened.path, notes.join("・")),
        ports: selected
            .ports
            .iter()
            .copied()
            .zip(selected.interfaces.iter().cloned())
            .collect(),
    };
    let service = PhcService::new(
        PhcConfig::with_poll_interval(TimeDelta::from_std(interval)),
        opened.port,
        client,
        selected.ports,
        Arc::clone(clock),
    );
    Ok((service, target))
}

/// 受信機の監視を別スレッドで始める。SNMP は `--transport` などの指定に関係なく、ふつうの UDP ソケットで送る。
fn spawn_receiver_monitor(
    target: SocketAddr,
    config: ReceiverMonitorConfig,
    clock: Arc<dyn ClockPort>,
    stop: &Arc<AtomicBool>,
) -> anyhow::Result<(mpsc::Receiver<ReceiverSnapshot>, thread::JoinHandle<()>)> {
    let port = UdpTransport::connect(target, Arc::clone(&clock))
        .with_context(|| format!("SNMP の送受信の準備ができません（{target}）"))?;
    let mut monitor = ReceiverMonitor::new(config, port, clock);
    let (tx, rx) = mpsc::channel();
    let handle = thread::Builder::new()
        .name("snmp-receiver".into())
        .spawn({
            let thread_stop = Arc::clone(stop);
            move || {
                // 受け手（UI）が終了した後は送れないが、監視もまもなく止まるので捨ててよい
                let mut publish = |snapshot: &ReceiverSnapshot| drop(tx.send(snapshot.clone()));
                monitor.run_until(&mut publish, &thread_stop);
            }
        })
        .context("受信機の監視スレッドを起動できません")?;
    Ok((rx, handle))
}

fn run_once(args: &OnceArgs) -> anyhow::Result<ExitCode> {
    args.ptp
        .check(args.target.server.as_deref(), &args.transport.given())?;
    if args.ptp.ptp4l {
        return run_once_phc(args);
    }
    let zone = args.zone.zone();
    let server = resolve(required_server(args.target.server.as_deref())?)?;
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
    let timestamping = Arc::new(TimestampingStatus::new());
    let OpenedTransport {
        port: transport,
        description: transport_label,
    } = args
        .transport
        .open(server, Arc::clone(&clock), Arc::clone(&timestamping))?;

    let result = query_once(
        transport,
        RandomNonce::new(),
        clock,
        RequestTemplate::default(),
        Duration::from_millis(args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS)),
    );
    match result {
        Ok(outcome) if args.target.json => {
            writeln!(
                io::stdout().lock(),
                "{}",
                once::render_json(
                    server,
                    &transport_label,
                    timestamping_label(&timestamping.counts()),
                    &outcome,
                )
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Ok(outcome) => {
            write!(
                io::stdout().lock(),
                "{}",
                once::render_text(
                    server,
                    &transport_label,
                    timestamping_label(&timestamping.counts()),
                    &outcome,
                    &zone,
                )
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Err(error) if args.target.json => {
            writeln!(
                io::stdout().lock(),
                "{}",
                once::render_error_json(server, &error)
            )?;
            Ok(ExitCode::FAILURE)
        }
        Err(error) => {
            writeln!(io::stderr().lock(), "{server}: {error}")?;
            Ok(ExitCode::FAILURE)
        }
    }
}

/// `once --ptp4l`: ptp4l の状態と PHC を 1 回読む。ptp4l が同期を知らせていなければ、読めても終了コード 1。
fn run_once_phc(args: &OnceArgs) -> anyhow::Result<ExitCode> {
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
    let timeout = Duration::from_millis(args.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
    let (mut service, target) = open_ptp4l(
        &args.ptp,
        &clock,
        Duration::from_millis(DEFAULT_INTERVAL_MS),
        timeout,
    )?;
    match service.poll_once() {
        Ok(snapshot) => {
            if args.target.json {
                writeln!(
                    io::stdout().lock(),
                    "{}",
                    phc::render_once_json(&target, &snapshot)
                )?;
            } else {
                write!(
                    io::stdout().lock(),
                    "{}",
                    phc::render_once_text(&target, &snapshot, &args.zone.zone())
                )?;
            }
            Ok(if snapshot.state.shows_time() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Err(error) if args.target.json => {
            writeln!(
                io::stdout().lock(),
                "{}",
                phc::render_once_error_json(&target, &error.to_string())
            )?;
            Ok(ExitCode::FAILURE)
        }
        Err(error) => {
            writeln!(io::stderr().lock(), "{}: {error}", target.phc)?;
            Ok(ExitCode::FAILURE)
        }
    }
}

fn run_stats(args: &StatsArgs) -> anyhow::Result<ExitCode> {
    let mut ntp_only = args.transport.given();
    ntp_only.push(("--allow-fast-poll", args.allow_fast_poll));
    args.ptp.check(args.target.server.as_deref(), &ntp_only)?;
    if args.ptp.ptp4l {
        return run_stats_phc(args);
    }
    let server = resolve(required_server(args.target.server.as_deref())?)?;
    let interval = polling_interval(args.interval_ms);
    let count = args.count.unwrap_or(DEFAULT_COUNT);
    if !args.allow_fast_poll {
        polling_policy::check_interval(server.ip(), interval)?;
    }
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
    let timestamping = Arc::new(TimestampingStatus::new());
    let OpenedTransport {
        port: transport,
        description: transport_label,
    } = args
        .transport
        .open(server, Arc::clone(&clock), Arc::clone(&timestamping))?;
    let mut service = SyncService::new(
        SourceConfig::with_poll_interval(TimeDelta::from_std(interval)),
        transport,
        RandomNonce::new(),
        clock,
        RequestTemplate::default(),
    );

    let mut reporter = StatsReporter::new(io::stdout().lock(), server, args.target.json)
        .with_timestamping(Arc::clone(&timestamping));
    if count == 0 {
        reporter = reporter.with_periodic_summary(20);
    }
    reporter.write_header(interval, &transport_label);
    service.run(&mut reporter, (count > 0).then_some(count));
    reporter.write_summary();
    Ok(ExitCode::SUCCESS)
}

/// `stats --ptp4l`: ptp4l の状態と PHC を一定間隔で読み続け、統計を出す。
fn run_stats_phc(args: &StatsArgs) -> anyhow::Result<ExitCode> {
    let interval = polling_interval(args.interval_ms);
    let count = args.count.unwrap_or(DEFAULT_COUNT);
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
    let (mut service, target) = open_ptp4l(
        &args.ptp,
        &clock,
        interval,
        Duration::from_millis(PTP4L_TIMEOUT_MS),
    )?;
    let mut reporter = PhcStatsReporter::new(io::stdout().lock(), target, args.target.json);
    if count == 0 {
        reporter = reporter.with_periodic_summary(20);
    }
    reporter.write_header(interval.as_millis());
    service.run_until(
        &mut reporter,
        (count > 0).then_some(count),
        &Arc::new(AtomicBool::new(false)),
    );
    reporter.write_summary();
    Ok(ExitCode::SUCCESS)
}

fn run_sim_server(args: &SimServerArgs) -> anyhow::Result<ExitCode> {
    let bind = args.bind.unwrap_or(DEFAULT_SIM_BIND);
    let offset_ms = args.offset_ms.unwrap_or(DEFAULT_SIM_OFFSET_MS);
    let stratum = args.stratum.unwrap_or(DEFAULT_SIM_STRATUM);
    let refid = args
        .refid
        .clone()
        .unwrap_or_else(|| RefId(DEFAULT_SIM_REFID.to_owned()));
    anyhow::ensure!(
        args.snmp_bind.is_none() || args.snmp_mib.is_some(),
        "--snmp-bind は --snmp-mib と一緒に指定してください"
    );
    let config = SimServerConfig {
        offset: TimeDelta::from_millis_f64(offset_ms),
        stratum,
        reference_id: refid.bytes(),
    };
    let server =
        SimServer::spawn(bind, config).with_context(|| format!("{bind} で待ち受けできません"))?;
    writeln!(
        io::stderr().lock(),
        "疑似 NTP サーバを {} で起動しました（offset {offset_ms:+.3} ms, stratum {stratum}, refid {refid}）。Ctrl+C で終了します。",
        server.local_addr(),
    )?;
    let agent = match args.snmp_mib {
        Some(mib) => {
            let snmp_bind = args.snmp_bind.unwrap_or(DEFAULT_SIM_SNMP_BIND);
            let agent = SimSnmpAgent::spawn(snmp_bind, mib.profile())
                .with_context(|| format!("{snmp_bind} で SNMP を待ち受けできません"))?;
            writeln!(
                io::stderr().lock(),
                "疑似 SNMP エージェントを {} で起動しました（{}、コミュニティ名 public）。",
                agent.local_addr(),
                mib.profile().mib_name(),
            )?;
            Some(agent)
        }
        None => None,
    };
    server.join();
    drop(agent);
    Ok(ExitCode::SUCCESS)
}

/// 引数か設定ファイルで指定したサーバ。
fn required_server(server: Option<&str>) -> anyhow::Result<&str> {
    server.context("NTP サーバを指定してください（引数か、設定ファイルの server）")
}

fn polling_interval(interval_ms: Option<u64>) -> Duration {
    Duration::from_millis(
        interval_ms
            .unwrap_or(DEFAULT_INTERVAL_MS)
            .max(MIN_INTERVAL_MS),
    )
}

fn resolve(server: &str) -> anyhow::Result<SocketAddr> {
    resolve_server(server).with_context(|| format!("サーバのアドレスを解決できません: {server}"))
}
