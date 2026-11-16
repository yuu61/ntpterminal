//! ntpterminal: GNSS タイムサーバのデモ用 NTP クロック。
//!
//! ここは組み立て役（composition root）。各層の部品をつなぐだけで、ロジックは持たない。

use std::io;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, bail};
use application::polling_policy;
use application::port::ClockPort;
use application::usecase::{SyncService, query_once};
use clap::{Args, Parser, Subcommand};
use domain::packet_crafting::RequestTemplate;
use domain::shared_kernel::TimeDelta;
use domain::time_sync::SourceConfig;
use infrastructure::clock::SystemClock;
use infrastructure::nonce::RandomNonce;
use infrastructure::resolve::resolve_server;
use infrastructure::sim_server::{SimServer, SimServerConfig};
use infrastructure::transport::UdpTransport;
use presentation::cli::once;
use presentation::cli::stats::StatsReporter;
use presentation::format::DisplayZone;
use presentation::tui::{self, SnapshotSender, TuiConfig};

#[derive(Parser)]
#[command(
    name = "ntpterminal",
    version,
    about = "GNSS タイムサーバのデモ用 NTP クロック"
)]
struct Cli {
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
    /// NTP サーバ（例: 192.168.92.234 / ntp.example.com / 127.0.0.1:12300）。ポート省略時は 123
    server: String,
    /// 結果を JSON で出力する
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ZoneArgs {
    /// 表示用のタイムゾーン（UTC からのずれ）
    #[arg(long, default_value = "+09:00", allow_hyphen_values = true)]
    tz: String,
    /// タイムゾーンの表示名
    #[arg(long, default_value = "JST")]
    tz_label: String,
}

impl ZoneArgs {
    fn zone(&self) -> anyhow::Result<DisplayZone> {
        let offset = DisplayZone::parse_offset(&self.tz).map_err(anyhow::Error::msg)?;
        Ok(DisplayZone::new(offset, self.tz_label.clone()))
    }
}

#[derive(Args)]
struct TuiArgs {
    /// NTP サーバ（例: 192.168.92.234 / 127.0.0.1:12300）。ポート省略時は 123
    server: String,
    /// ポーリング間隔（ミリ秒）
    #[arg(long, default_value_t = 500)]
    interval_ms: u64,
    /// 公開サーバへの短い間隔のポーリングを許可する（自分で管理するサーバに限ること）
    #[arg(long)]
    allow_fast_poll: bool,
    /// 最上部の見出し
    #[arg(long, default_value = "NTP CLOCK")]
    title: String,
    /// 最下部に出す一文（スポンサー表記など）
    #[arg(long)]
    footer: Option<String>,
    #[command(flatten)]
    zone: ZoneArgs,
}

#[derive(Args)]
struct OnceArgs {
    #[command(flatten)]
    target: TargetArgs,
    /// 応答を待つ上限（ミリ秒）
    #[arg(long, default_value_t = 1000)]
    timeout_ms: u64,
    #[command(flatten)]
    zone: ZoneArgs,
}

#[derive(Args)]
struct StatsArgs {
    #[command(flatten)]
    target: TargetArgs,
    /// ポーリング間隔（ミリ秒）
    #[arg(long, default_value_t = 500)]
    interval_ms: u64,
    /// 試行回数（0 = 無制限。Ctrl+C で終了）
    #[arg(long, default_value_t = 20)]
    count: u64,
    /// 公開サーバへの短い間隔のポーリングを許可する（自分で管理するサーバに限ること）
    #[arg(long)]
    allow_fast_poll: bool,
}

#[derive(Args)]
struct SimServerArgs {
    /// 待ち受けるアドレス
    #[arg(long, default_value = "127.0.0.1:12300")]
    bind: SocketAddr,
    /// PC の時計に対するサーバ時刻のずれ（ミリ秒。負も可）
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    offset_ms: f64,
    /// 応答に入れる stratum
    #[arg(long, default_value_t = 1)]
    stratum: u8,
    /// 応答に入れる Reference ID（4 文字以内の ASCII）
    #[arg(long, default_value = "SIM")]
    refid: String,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("エラー: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.command {
        Command::Tui(args) => run_tui(&args),
        Command::Once(args) => run_once(&args),
        Command::Stats(args) => run_stats(&args),
        Command::SimServer(args) => run_sim_server(&args),
    }
}

fn run_tui(args: &TuiArgs) -> anyhow::Result<ExitCode> {
    let zone = args.zone.zone()?;
    let server = resolve(&args.server)?;
    let interval = Duration::from_millis(args.interval_ms.max(10));
    if !args.allow_fast_poll {
        polling_policy::check_interval(server.ip(), interval)?;
    }
    // 同期スレッドと UI は必ず同じ時計インスタンスを使う。
    // モノトニック時計の起点（作成時の Instant）が違うと、基準点からの計算が黙って狂う
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
    let transport =
        UdpTransport::connect(server, Arc::clone(&clock)).context("UDP ソケットを開けません")?;
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
            let stop = Arc::clone(&stop);
            move || service.run_until(&mut SnapshotSender::new(tx), None, &stop)
        })
        .context("同期スレッドを起動できません")?;

    let config = TuiConfig {
        title: args.title.clone(),
        server_label: args.server.trim().to_string(),
        footer: args.footer.clone(),
        zone,
    };
    let result = tui::run(&config, clock, rx);

    stop.store(true, Ordering::Relaxed);
    let _ = sync_thread.join();
    result.context("端末の表示でエラーが起きました")?;
    Ok(ExitCode::SUCCESS)
}

fn run_once(args: &OnceArgs) -> anyhow::Result<ExitCode> {
    let zone = args.zone.zone()?;
    let server = resolve(&args.target.server)?;
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
    let transport =
        UdpTransport::connect(server, Arc::clone(&clock)).context("UDP ソケットを開けません")?;

    let result = query_once(
        transport,
        RandomNonce::new(),
        clock,
        RequestTemplate::default(),
        Duration::from_millis(args.timeout_ms),
    );
    match result {
        Ok(outcome) if args.target.json => {
            println!("{}", once::render_json(server, &outcome));
            Ok(ExitCode::SUCCESS)
        }
        Ok(outcome) => {
            print!("{}", once::render_text(server, &outcome, &zone));
            Ok(ExitCode::SUCCESS)
        }
        Err(error) if args.target.json => {
            println!("{}", once::render_error_json(server, &error));
            Ok(ExitCode::FAILURE)
        }
        Err(error) => {
            eprintln!("{server}: {error}");
            Ok(ExitCode::FAILURE)
        }
    }
}

fn run_stats(args: &StatsArgs) -> anyhow::Result<ExitCode> {
    let server = resolve(&args.target.server)?;
    let interval = Duration::from_millis(args.interval_ms.max(10));
    if !args.allow_fast_poll {
        polling_policy::check_interval(server.ip(), interval)?;
    }
    let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
    let transport =
        UdpTransport::connect(server, Arc::clone(&clock)).context("UDP ソケットを開けません")?;
    let mut service = SyncService::new(
        SourceConfig::with_poll_interval(TimeDelta::from_std(interval)),
        transport,
        RandomNonce::new(),
        clock,
        RequestTemplate::default(),
    );

    let mut reporter = StatsReporter::new(io::stdout().lock(), server, args.target.json);
    if args.count == 0 {
        reporter = reporter.with_periodic_summary(20);
    }
    reporter.write_header(interval);
    service.run(&mut reporter, (args.count > 0).then_some(args.count));
    reporter.write_summary();
    Ok(ExitCode::SUCCESS)
}

fn run_sim_server(args: &SimServerArgs) -> anyhow::Result<ExitCode> {
    let config = SimServerConfig {
        offset: TimeDelta::from_millis_f64(args.offset_ms),
        stratum: args.stratum,
        reference_id: refid_bytes(&args.refid)?,
    };
    let server = SimServer::spawn(args.bind, config)
        .with_context(|| format!("{} で待ち受けできません", args.bind))?;
    eprintln!(
        "疑似 NTP サーバを {} で起動しました（offset {:+.3} ms, stratum {}, refid {}）。Ctrl+C で終了します。",
        server.local_addr(),
        args.offset_ms,
        args.stratum,
        args.refid
    );
    server.join();
    Ok(ExitCode::SUCCESS)
}

fn resolve(server: &str) -> anyhow::Result<SocketAddr> {
    resolve_server(server).with_context(|| format!("サーバのアドレスを解決できません: {server}"))
}

fn refid_bytes(text: &str) -> anyhow::Result<[u8; 4]> {
    if !text.is_ascii() || text.len() > 4 {
        bail!("Reference ID は 4 文字以内の ASCII で指定してください: {text}");
    }
    let mut bytes = [0u8; 4];
    bytes[..text.len()].copy_from_slice(text.as_bytes());
    Ok(bytes)
}
