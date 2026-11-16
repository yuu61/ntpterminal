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
use std::process::ExitCode;
use std::sync::mpsc;
use std::thread;

use anyhow::Context as _;
use application::polling_policy;
use application::port::ClockPort;
use application::timestamping::TimestampingStatus;
use application::usecase::{SyncService, query_once};
use clap::{Args, Parser, Subcommand, ValueEnum};
use domain::packet_crafting::{
    EthernetSpec, HeaderTemplate, Ipv4Spec, MacAddr, RequestTemplate, UdpChecksum, VlanTag,
};
use domain::shared_kernel::TimeDelta;
use domain::time_sync::SourceConfig;
use infrastructure::clock::SystemClock;
use infrastructure::nonce::RandomNonce;
use infrastructure::resolve::resolve_server;
use infrastructure::sim_server::{SimServer, SimServerConfig};
use infrastructure::transport::{
    self as transport_layer, LinkOptions, OpenedTransport, TimestampingMode, TransportMode,
    TransportOptions,
};
use presentation::cli::once;
use presentation::cli::stats::StatsReporter;
use presentation::format::{DisplayZone, timestamping_label};
use presentation::tui::{self, SnapshotSender, TuiConfig};
use serde::Deserialize;

use crate::config::ConfigFile;

/// `--config` を省略したときに読む設定ファイル（カレントディレクトリ。なければ読まない）
const DEFAULT_CONFIG_PATH: &str = "config.toml";

// ---- 既定値（コマンドラインにも設定ファイルにもないときに使う。ヘルプの「既定:」と揃える） ----
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
    zone: ZoneArgs,
    #[command(flatten)]
    transport: TransportArgs,
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
    let zone = args.zone.zone();
    let server_text = required_server(args.server.as_deref())?;
    let server = resolve(server_text)?;
    let interval = polling_interval(args.interval_ms);
    if !args.allow_fast_poll {
        polling_policy::check_interval(server.ip(), interval)?;
    }
    // 同期スレッドと UI は必ず同じ時計インスタンスを使う。
    // モノトニック時計の起点（作成時の Instant）が違うと、基準点からの計算が黙って狂う
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
        Arc::clone(&clock),
        RequestTemplate::default(),
    );

    // 同期は別スレッドで回し、スナップショットだけをチャネルで UI に渡す（集約は共有しない）
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let sync_thread = thread::Builder::new()
        .name("ntp-sync".into())
        .spawn({
            let thread_stop = Arc::clone(&stop);
            move || service.run_until(&mut SnapshotSender::new(tx), None, &thread_stop)
        })
        .context("同期スレッドを起動できません")?;

    let config = TuiConfig {
        title: args
            .title
            .clone()
            .unwrap_or_else(|| DEFAULT_TITLE.to_owned()),
        server_label: server_text.trim().to_owned(),
        footer: args.footer.clone(),
        trend: args.trend,
        zone,
        transport: transport_label,
        timestamping,
    };
    let result = tui::run(&config, clock.as_ref(), &rx);

    stop.store(true, Ordering::Relaxed);
    drop(sync_thread.join());
    result.context("端末の表示でエラーが起きました")?;
    Ok(ExitCode::SUCCESS)
}

fn run_once(args: &OnceArgs) -> anyhow::Result<ExitCode> {
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

fn run_stats(args: &StatsArgs) -> anyhow::Result<ExitCode> {
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

fn run_sim_server(args: &SimServerArgs) -> anyhow::Result<ExitCode> {
    let bind = args.bind.unwrap_or(DEFAULT_SIM_BIND);
    let offset_ms = args.offset_ms.unwrap_or(DEFAULT_SIM_OFFSET_MS);
    let stratum = args.stratum.unwrap_or(DEFAULT_SIM_STRATUM);
    let refid = args
        .refid
        .clone()
        .unwrap_or_else(|| RefId(DEFAULT_SIM_REFID.to_owned()));
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
    server.join();
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
