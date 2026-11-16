//! 設定ファイル（TOML）。`--config <PATH>` で指定したファイルを読む。
//!
//! - `--config` を省略したときは、カレントディレクトリの `config.toml` を読む。なければ設定ファイルなし
//!   （コマンドラインと既定値だけ）として扱い、あるのに読めない・内容が正しくないときはエラーにする。
//!   手元の `config.toml` を読ませたくない（スクリプトなど）ときは `--config /dev/null`（空の設定。Windows では `NUL`）を渡す
//! - キーはコマンドラインの長いオプションと同じ名前で、値の書き方も同じ（`--interval-ms 200` → `interval-ms = 200`）。
//!   tui / once / stats に共通のもの（サーバ・タイムゾーン・送信方式・ptp4l の PHC・PTP のマスタ）は最上位に、
//!   そのサブコマンドだけのものは `[tui]` などの節に書く。全項目の例は `docs/config.example.toml`、
//!   各項目の値と既定値は `docs/configuration.md`
//! - GNSS 受信機を SNMP で読む指定（`snmp-community` など）は `[tui]` に書く。コミュニティ名を書いたファイルは
//!   ほかの人に読ませない
//! - 優先順位はコマンドライン → 設定ファイル → 既定値。真偽の項目（`trend` / `json` / `no-df` / `ptp4l` / `ptp`）は
//!   コマンドラインに付けるオプションしかないので、設定ファイルで true にしたものはコマンドラインで戻せない
//! - 知らないキーはエラーにする（綴りの誤りを黙って無視しない）。値の範囲と形式はコマンドラインと同じ関数で
//!   確かめ、誤りは行と列を添えて知らせる。値どうしの組み合わせ（udp で `dscp` など）は、コマンドラインの値と
//!   合わせた後で、コマンドラインと同じ場所（送受信の準備）で確かめる
//! - `--allow-fast-poll` は書けない。設定ファイルで許すと、コマンドラインで公開サーバを指定したときにも
//!   短い間隔で問い合わせてしまうため、毎回コマンドラインで指定する

use core::fmt::Display;
use core::net::{Ipv4Addr, SocketAddr};
use core::str::FromStr as _;
use std::path::PathBuf;

use application::usecase::PhcRequest;
use domain::packet_crafting::{MacAddr, UdpChecksum};
use presentation::format::DisplayZone;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use toml::de::Error as TomlError;

use crate::{
    Command, DSCP_MAX, ECN_MAX, MibArg, ModeArg, OnceArgs, PCP_MAX, PtpArgs, RefId, SimMibArg,
    SimPtpMasterArgs, SimServerArgs, SnmpArgs, StatsArgs, TargetArgs, TimestampingArg,
    TransportArgs, TuiArgs, VLAN_ID_MAX, ZoneArgs, parse_hex_u16, parse_phc, parse_ptp_address,
    parse_udp_checksum,
};

/// 設定ファイルの内容。どの項目も省略でき、省略したものはコマンドラインの値か既定値になる。
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub(crate) struct ConfigFile {
    // ---- tui / once / stats に共通 ----
    server: Option<String>,
    #[serde(deserialize_with = "tz")]
    tz: Option<i32>,
    tz_label: Option<String>,
    transport: Option<ModeArg>,
    timestamping: Option<TimestampingArg>,
    source_port: Option<u16>,
    ttl: Option<u8>,
    #[serde(deserialize_with = "dscp")]
    dscp: Option<u8>,
    #[serde(deserialize_with = "ecn")]
    ecn: Option<u8>,
    ip_id: Option<u16>,
    no_df: Option<bool>,
    #[serde(deserialize_with = "udp_checksum")]
    udp_checksum: Option<UdpChecksum>,
    #[serde(deserialize_with = "ip_checksum")]
    ip_checksum: Option<u16>,
    #[serde(deserialize_with = "dst_mac")]
    dst_mac: Option<MacAddr>,
    #[serde(deserialize_with = "vlan")]
    vlan: Option<u16>,
    #[serde(deserialize_with = "pcp")]
    pcp: Option<u8>,
    // ---- tui / once / stats に共通: ptp4l が合わせる PHC ----
    ptp4l: Option<bool>,
    ptp4l_socket: Option<PathBuf>,
    ptp_domain: Option<u8>,
    #[serde(deserialize_with = "phc")]
    phc: Option<PhcRequest>,
    // ---- tui / once / stats に共通: PTP のマスタ（スレーブとして問い合わせる） ----
    ptp: Option<bool>,
    ptp_interface: Option<String>,
    #[serde(deserialize_with = "ptp_address")]
    ptp_master: Option<SocketAddr>,
    ptp_port: Option<u16>,
    // ---- サブコマンドごと ----
    tui: TuiSection,
    once: OnceSection,
    stats: StatsSection,
    sim_server: SimServerSection,
    sim_ptp_master: SimPtpMasterSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct TuiSection {
    interval_ms: Option<u64>,
    title: Option<String>,
    footer: Option<String>,
    trend: Option<bool>,
    snmp_community: Option<String>,
    snmp_port: Option<u16>,
    snmp_interval_ms: Option<u64>,
    snmp_mib: Option<MibArg>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct OnceSection {
    timeout_ms: Option<u64>,
    json: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct StatsSection {
    interval_ms: Option<u64>,
    count: Option<u64>,
    json: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct SimServerSection {
    bind: Option<SocketAddr>,
    offset_ms: Option<f64>,
    stratum: Option<u8>,
    #[serde(deserialize_with = "refid")]
    refid: Option<RefId>,
    snmp_mib: Option<SimMibArg>,
    snmp_bind: Option<SocketAddr>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct SimPtpMasterSection {
    port: Option<u16>,
    #[serde(deserialize_with = "ptp_address")]
    to: Option<SocketAddr>,
    interface: Option<Ipv4Addr>,
    domain: Option<u8>,
    offset_ms: Option<f64>,
    one_step: Option<bool>,
    #[serde(deserialize_with = "log_sync_interval")]
    log_sync_interval: Option<i8>,
}

/// 設定ファイルの中身を読む。
///
/// # Errors
///
/// TOML として読めないとき、知らないキーがあるとき、値の型・範囲・形式が違うとき。
/// エラーの文言には行と列が入る。
pub(crate) fn parse(text: &str) -> Result<ConfigFile, TomlError> {
    toml::from_str(text)
}

// 各 `fill_*` は `let … { .. } = args` ですべての項目を取り出す（項目を足すと、ここを直すまでコンパイルできない）。
impl ConfigFile {
    /// コマンドラインで指定しなかった項目を、設定ファイルの値で埋める。
    pub(crate) fn fill(&self, command: Command) -> Command {
        match command {
            Command::Tui(args) => Command::Tui(self.fill_tui(args)),
            Command::Once(args) => Command::Once(self.fill_once(args)),
            Command::Stats(args) => Command::Stats(self.fill_stats(args)),
            Command::SimServer(args) => Command::SimServer(self.fill_sim_server(args)),
            Command::SimPtpMaster(args) => Command::SimPtpMaster(self.fill_sim_ptp_master(args)),
        }
    }

    fn fill_tui(&self, args: TuiArgs) -> TuiArgs {
        let TuiArgs {
            server,
            interval_ms,
            allow_fast_poll,
            title,
            footer,
            trend,
            snmp,
            zone,
            transport,
            ptp,
        } = args;
        let section = &self.tui;
        let SnmpArgs {
            community,
            port,
            interval_ms: snmp_interval_ms,
            mib,
        } = snmp;
        TuiArgs {
            server: server.or_else(|| self.server.clone()),
            interval_ms: interval_ms.or(section.interval_ms),
            // 設定ファイルには書けない（モジュールの説明を参照）
            allow_fast_poll,
            title: title.or_else(|| section.title.clone()),
            footer: footer.or_else(|| section.footer.clone()),
            trend: trend || section.trend.unwrap_or(false),
            snmp: SnmpArgs {
                community: community.or_else(|| section.snmp_community.clone()),
                port: port.or(section.snmp_port),
                interval_ms: snmp_interval_ms.or(section.snmp_interval_ms),
                mib: mib.or(section.snmp_mib),
            },
            zone: self.fill_zone(zone),
            transport: self.fill_transport(transport),
            ptp: self.fill_ptp(ptp),
        }
    }

    fn fill_once(&self, args: OnceArgs) -> OnceArgs {
        let OnceArgs {
            target,
            timeout_ms,
            zone,
            transport,
            ptp,
        } = args;
        let section = &self.once;
        OnceArgs {
            target: self.fill_target(target, section.json),
            timeout_ms: timeout_ms.or(section.timeout_ms),
            zone: self.fill_zone(zone),
            transport: self.fill_transport(transport),
            ptp: self.fill_ptp(ptp),
        }
    }

    fn fill_stats(&self, args: StatsArgs) -> StatsArgs {
        let StatsArgs {
            target,
            interval_ms,
            count,
            allow_fast_poll,
            transport,
            ptp,
        } = args;
        let section = &self.stats;
        StatsArgs {
            target: self.fill_target(target, section.json),
            interval_ms: interval_ms.or(section.interval_ms),
            count: count.or(section.count),
            // 設定ファイルには書けない（モジュールの説明を参照）
            allow_fast_poll,
            transport: self.fill_transport(transport),
            ptp: self.fill_ptp(ptp),
        }
    }

    fn fill_sim_server(&self, args: SimServerArgs) -> SimServerArgs {
        let SimServerArgs {
            bind,
            offset_ms,
            stratum,
            refid,
            snmp_mib,
            snmp_bind,
        } = args;
        let section = &self.sim_server;
        SimServerArgs {
            bind: bind.or(section.bind),
            offset_ms: offset_ms.or(section.offset_ms),
            stratum: stratum.or(section.stratum),
            refid: refid.or_else(|| section.refid.clone()),
            snmp_mib: snmp_mib.or(section.snmp_mib),
            snmp_bind: snmp_bind.or(section.snmp_bind),
        }
    }

    fn fill_sim_ptp_master(&self, args: SimPtpMasterArgs) -> SimPtpMasterArgs {
        let SimPtpMasterArgs {
            port,
            to,
            interface,
            domain,
            offset_ms,
            one_step,
            log_sync_interval,
        } = args;
        let section = &self.sim_ptp_master;
        SimPtpMasterArgs {
            port: port.or(section.port),
            to: to.or(section.to),
            interface: interface.or(section.interface),
            domain: domain.or(section.domain),
            offset_ms: offset_ms.or(section.offset_ms),
            one_step: one_step || section.one_step.unwrap_or(false),
            log_sync_interval: log_sync_interval.or(section.log_sync_interval),
        }
    }

    /// `section_json` はサブコマンドの節（`[once]` / `[stats]`）の値。
    fn fill_target(&self, args: TargetArgs, section_json: Option<bool>) -> TargetArgs {
        let TargetArgs { server, json } = args;
        TargetArgs {
            server: server.or_else(|| self.server.clone()),
            json: json || section_json.unwrap_or(false),
        }
    }

    fn fill_zone(&self, args: ZoneArgs) -> ZoneArgs {
        let ZoneArgs { tz, tz_label } = args;
        ZoneArgs {
            tz: tz.or(self.tz),
            tz_label: tz_label.or_else(|| self.tz_label.clone()),
        }
    }

    fn fill_ptp(&self, args: PtpArgs) -> PtpArgs {
        let PtpArgs {
            ptp4l,
            ptp,
            ptp4l_socket,
            ptp_domain,
            phc,
            ptp_interface,
            ptp_master,
            ptp_port,
        } = args;
        PtpArgs {
            ptp4l: ptp4l || self.ptp4l.unwrap_or(false),
            ptp: ptp || self.ptp.unwrap_or(false),
            ptp4l_socket: ptp4l_socket.or_else(|| self.ptp4l_socket.clone()),
            ptp_domain: ptp_domain.or(self.ptp_domain),
            phc: phc.or_else(|| self.phc.clone()),
            ptp_interface: ptp_interface.or_else(|| self.ptp_interface.clone()),
            ptp_master: ptp_master.or(self.ptp_master),
            ptp_port: ptp_port.or(self.ptp_port),
        }
    }

    fn fill_transport(&self, args: TransportArgs) -> TransportArgs {
        let TransportArgs {
            mode,
            timestamping,
            source_port,
            ttl,
            dscp,
            ecn,
            ip_id,
            no_df,
            udp_checksum,
            ip_checksum,
            dst_mac,
            vlan,
            pcp,
        } = args;
        TransportArgs {
            mode: mode.or(self.transport),
            timestamping: timestamping.or(self.timestamping),
            source_port: source_port.or(self.source_port),
            ttl: ttl.or(self.ttl),
            dscp: dscp.or(self.dscp),
            ecn: ecn.or(self.ecn),
            ip_id: ip_id.or(self.ip_id),
            no_df: no_df || self.no_df.unwrap_or(false),
            udp_checksum: udp_checksum.or(self.udp_checksum),
            ip_checksum: ip_checksum.or(self.ip_checksum),
            dst_mac: dst_mac.or(self.dst_mac),
            vlan: vlan.or(self.vlan),
            pcp: pcp.or(self.pcp),
        }
    }
}

// ---- 値の確かめ（コマンドラインと同じ関数・同じ範囲を使う） ----

/// 文字列を `parse_text` で読む。失敗の文言はそのまま設定ファイルのエラーにする。
fn parsed<'de, D, T, E, F>(deserializer: D, parse_text: F) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    E: Display,
    F: FnOnce(&str) -> Result<T, E>,
{
    let text = String::deserialize(deserializer)?;
    parse_text(&text).map(Some).map_err(D::Error::custom)
}

/// 0 から `max` までの整数を読む。
fn up_to<'de, D, T>(deserializer: D, max: T) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + PartialOrd + Display + Copy,
{
    let value = T::deserialize(deserializer)?;
    if value > max {
        return Err(D::Error::custom(format!(
            "0〜{max} で指定してください: {value}"
        )));
    }
    Ok(Some(value))
}

fn tz<'de, D>(deserializer: D) -> Result<Option<i32>, D::Error>
where
    D: Deserializer<'de>,
{
    parsed(deserializer, DisplayZone::parse_offset)
}

fn dscp<'de, D>(deserializer: D) -> Result<Option<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    up_to(deserializer, DSCP_MAX)
}

fn ecn<'de, D>(deserializer: D) -> Result<Option<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    up_to(deserializer, ECN_MAX)
}

fn udp_checksum<'de, D>(deserializer: D) -> Result<Option<UdpChecksum>, D::Error>
where
    D: Deserializer<'de>,
{
    parsed(deserializer, parse_udp_checksum)
}

fn ip_checksum<'de, D>(deserializer: D) -> Result<Option<u16>, D::Error>
where
    D: Deserializer<'de>,
{
    parsed(deserializer, parse_hex_u16)
}

fn dst_mac<'de, D>(deserializer: D) -> Result<Option<MacAddr>, D::Error>
where
    D: Deserializer<'de>,
{
    parsed(deserializer, MacAddr::from_str)
}

fn vlan<'de, D>(deserializer: D) -> Result<Option<u16>, D::Error>
where
    D: Deserializer<'de>,
{
    up_to(deserializer, VLAN_ID_MAX)
}

fn pcp<'de, D>(deserializer: D) -> Result<Option<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    up_to(deserializer, PCP_MAX)
}

fn phc<'de, D>(deserializer: D) -> Result<Option<PhcRequest>, D::Error>
where
    D: Deserializer<'de>,
{
    parsed(deserializer, parse_phc)
}

fn ptp_address<'de, D>(deserializer: D) -> Result<Option<SocketAddr>, D::Error>
where
    D: Deserializer<'de>,
{
    parsed(deserializer, parse_ptp_address)
}

/// Sync の間隔の log2（-7〜4。コマンドラインと同じ範囲）。
fn log_sync_interval<'de, D>(deserializer: D) -> Result<Option<i8>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = i8::deserialize(deserializer)?;
    if !(-7..=4).contains(&value) {
        return Err(D::Error::custom(format!(
            "-7〜4 で指定してください: {value}"
        )));
    }
    Ok(Some(value))
}

fn refid<'de, D>(deserializer: D) -> Result<Option<RefId>, D::Error>
where
    D: Deserializer<'de>,
{
    parsed(deserializer, RefId::from_str)
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::iter;
    use core::time::Duration;

    use application::acl::mib::MibProfile;
    use application::port::ClockPort;
    use application::timestamping::TimestampingStatus;
    use clap::{CommandFactory as _, Parser as _, ValueEnum as _};
    use infrastructure::clock::SystemClock;
    use infrastructure::transport::TransportMode;

    use super::*;
    use std::path::Path;

    use crate::{Cli, load_config, load_config_if_exists, polling_interval, required_server};

    /// 全項目の例。テストでは `# key = value` の行のコメントを外し、すべての項目を有効にして読む。
    const EXAMPLE: &str = include_str!("../docs/config.example.toml");

    /// 項目の説明。
    const REFERENCE: &str = include_str!("../docs/configuration.md");

    /// 設定ファイルに書かないオプション（`help` / `version` は clap が足すもの）。
    const NOT_IN_FILE: [&str; 4] = ["allow-fast-poll", "config", "help", "version"];

    fn uncomment_settings(text: &str) -> String {
        text.lines()
            .map(|line| match line.strip_prefix("# ") {
                Some(setting) if is_setting(setting) => setting,
                _ => line,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `key = value` の形か（キーは英小文字で始まり、英小文字・数字・`-` だけ。説明のコメントと区別する）。
    fn is_setting(text: &str) -> bool {
        text.split_once(" = ").is_some_and(|(key, _)| {
            key.starts_with(|ch: char| ch.is_ascii_lowercase())
                && key
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
        })
    }

    /// コマンドライン（プログラム名を除く）を読み、設定ファイルの値で埋める。
    fn merged(file: &str, args: &[&str]) -> Command {
        let cli =
            Cli::try_parse_from(iter::once("ntpterminal").chain(args.iter().copied())).unwrap();
        parse(file).unwrap().fill(cli.command)
    }

    fn parse_error(file: &str) -> String {
        parse(file).unwrap_err().to_string()
    }

    #[test]
    fn example_is_valid_as_is_and_with_every_setting_enabled() {
        // そのまま写しても（すべてコメントなので）何も変わらない
        assert_eq!(parse(EXAMPLE).unwrap().server, None);

        let file = parse(&uncomment_settings(EXAMPLE)).unwrap();
        assert_eq!(file.server.as_deref(), Some("192.168.1.123"));
        assert_eq!(file.tz, Some(DisplayZone::jst().offset_secs()));
        assert_eq!(file.transport, Some(ModeArg::Link));
        assert_eq!(file.dscp, Some(46));
        assert_eq!(file.udp_checksum, Some(UdpChecksum::Compute));
        assert_eq!(file.ip_checksum, Some(0x1234));
        assert_eq!(
            file.dst_mac,
            Some(MacAddr([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]))
        );
        assert_eq!(file.tui.trend, Some(true));
        assert_eq!(file.stats.count, Some(20));
        assert_eq!(file.sim_server.offset_ms, Some(-0.978f64));
        assert_eq!(file.sim_server.refid, Some(RefId("GNSS".to_owned())));
        assert_eq!(file.tui.snmp_community.as_deref(), Some("public"));
        assert_eq!(file.tui.snmp_mib, Some(MibArg::Auto));
        assert_eq!(file.sim_server.snmp_mib, Some(SimMibArg::MeinbergLtng));
        assert_eq!(file.ptp4l, Some(true));
        assert_eq!(file.ptp_domain, Some(0));
        assert_eq!(file.phc, Some(PhcRequest::Index(0)));
    }

    #[test]
    fn ptp4l_settings_merge_and_conflicts_are_errors() {
        let Command::Tui(args) = merged(
            "ptp4l = true
phc = \"eno1\"
",
            &["tui"],
        ) else {
            panic!("tui のはず");
        };
        assert!(args.ptp.ptp4l);
        assert_eq!(args.ptp.phc, Some(PhcRequest::Interface("eno1".to_owned())));
        args.ptp.check(None, &args.transport.given()).unwrap();

        // ptp4l とサーバ・送信方式は一緒に使えない（設定ファイルの値でも）
        let Command::Once(once) = merged(
            "ptp4l = true
transport = \"udp\"
",
            &["once", "192.168.1.123"],
        ) else {
            panic!("once のはず");
        };
        let server = once.ptp.check(once.target.server.as_deref(), &[]);
        assert!(server.unwrap_err().to_string().contains("NTP サーバ"));
        let transport = once.ptp.check(None, &once.transport.given());
        assert!(transport.unwrap_err().to_string().contains("--transport"));

        // ptp4l だけの指定は ptp4l なしではエラー
        let Command::Stats(stats) = merged(
            "ptp-domain = 24
",
            &["stats"],
        ) else {
            panic!("stats のはず");
        };
        let alone = stats.ptp.check(None, &[]).unwrap_err().to_string();
        assert!(alone.contains("--ptp-domain"), "{alone}");

        assert!(
            parse_error(
                "phc = \"/dev/ptpX\"
"
            )
            .contains("/dev/ptp0 の形")
        );
        // 数字だけは /dev/ptpN の書き間違い（インターフェース名にしない）
        assert!(
            parse_error(
                "phc = \"1\"
"
            )
            .contains("/dev/ptp1 の形")
        );
    }

    #[test]
    fn ptp_master_settings_merge_and_conflicts_are_errors() {
        let Command::Tui(args) = merged(
            "ptp = true
ptp-interface = \"eth0\"
ptp-master = \"192.168.1.50\"
timestamping = \"kernel\"
",
            &["tui", "--ptp-port", "12319"],
        ) else {
            panic!("tui のはず");
        };
        assert!(args.ptp.ptp);
        assert_eq!(args.ptp.ptp_interface.as_deref(), Some("eth0"));
        assert_eq!(args.ptp.ptp_master, Some("192.168.1.50:0".parse().unwrap()));
        assert_eq!(args.ptp.ptp_port, Some(12319));
        // --ptp では --timestamping は使える
        args.ptp.check(None, &args.transport.given()).unwrap();

        // --ptp とサーバ・送信方式・--ptp4l の項目は一緒に使えない
        for (file, command, expected) in [
            (
                "ptp = true
",
                &["once", "192.168.1.123"][..],
                "NTP サーバ",
            ),
            (
                "ptp = true
transport = \"udp\"
",
                &["once"],
                "--transport",
            ),
            (
                "ptp = true
phc = \"/dev/ptp0\"
",
                &["once"],
                "--phc は --ptp4l",
            ),
            (
                "ptp = true
ptp4l = true
",
                &["once"],
                "一緒に指定できません",
            ),
            (
                "ptp-port = 319
",
                &["once"],
                "--ptp-port は --ptp",
            ),
            (
                "ptp4l = true
ptp-master = \"10.0.0.1\"
",
                &["once"],
                "--ptp-master は --ptp",
            ),
        ] {
            let Command::Once(once) = merged(file, command) else {
                panic!("once のはず");
            };
            let error = once
                .ptp
                .check(once.target.server.as_deref(), &once.transport.given())
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{file}: {error}");
        }

        assert!(
            parse_error(
                "ptp-master = \"fe80::1\"
"
            )
            .contains("IPv4")
        );
        assert!(
            parse_error(
                "ptp-master = \"master.local\"
"
            )
            .contains("IPv4 アドレス")
        );
    }

    #[test]
    fn sim_ptp_master_settings_merge() {
        assert!(
            parse_error(
                "[sim-ptp-master]
log-sync-interval = 5
"
            )
            .contains("-7〜4")
        );
        let Command::SimPtpMaster(sim) = merged(
            "[sim-ptp-master]
to = \"127.0.0.1:13319\"
one-step = true
",
            &["sim-ptp-master", "--port", "12319"],
        ) else {
            panic!("sim-ptp-master のはず");
        };
        assert_eq!(sim.port, Some(12319));
        assert_eq!(sim.to, Some("127.0.0.1:13319".parse().unwrap()));
        assert!(sim.one_step);
    }

    /// オプションを足したのに設定ファイル（と例）に足し忘れることを防ぐ。
    /// 例はすべての項目を有効にしても `parse` できる（上のテスト）ので、例にあるキーは設定ファイルで使える。
    #[test]
    fn example_lists_every_command_line_option() {
        let example: toml::Table = toml::from_str(&uncomment_settings(EXAMPLE)).unwrap();
        for subcommand in Cli::command().get_subcommands() {
            let section = example
                .get(subcommand.get_name())
                .and_then(toml::Value::as_table);
            for arg in subcommand.get_arguments() {
                let key = arg.get_long().unwrap_or_else(|| arg.get_id().as_str());
                if NOT_IN_FILE.contains(&key) {
                    continue;
                }
                assert!(
                    example.contains_key(key)
                        || section.is_some_and(|table| table.contains_key(key)),
                    "{} の {key} が docs/config.example.toml にない",
                    subcommand.get_name()
                );
            }
        }
    }

    /// オプションを足したのに項目一覧（docs/configuration.md）に足し忘れることを防ぐ。
    /// 一覧には、コマンドラインの書き方（`--key`）と設定ファイルのキー（`key`）の両方を載せる。
    #[test]
    fn reference_lists_every_command_line_option() {
        let cli = Cli::command();
        // 最上位（`--config`）とサブコマンド
        for subcommand in iter::once(&cli).chain(cli.get_subcommands()) {
            for arg in subcommand.get_arguments() {
                let mut written = Vec::new();
                if let Some(long) = arg.get_long() {
                    if ["help", "version"].contains(&long) {
                        continue;
                    }
                    written.push(format!("`--{long}`"));
                }
                // 位置引数（サーバ）は設定ファイルのキーだけを載せる
                let key = arg.get_long().unwrap_or_else(|| arg.get_id().as_str());
                if !NOT_IN_FILE.contains(&key) {
                    written.push(format!("`{key}`"));
                }
                for text in written {
                    assert!(
                        REFERENCE.contains(&text),
                        "{} の {text} が docs/configuration.md にない",
                        subcommand.get_name()
                    );
                }
            }
        }
    }

    #[test]
    fn command_line_wins_over_file_and_file_wins_over_default() {
        let file = r#"
server = "192.168.1.123"
tz-label = "UTC"

[tui]
interval-ms = 200
title = "BOOTH"
"#;
        let Command::Tui(tui) = merged(file, &["tui", "10.0.0.1", "--interval-ms", "100"]) else {
            panic!("tui のはず");
        };
        // コマンドラインで指定したもの
        assert_eq!(tui.server.as_deref(), Some("10.0.0.1"));
        assert_eq!(
            polling_interval(tui.interval_ms),
            Duration::from_millis(100)
        );
        // 設定ファイルにだけあるもの
        assert_eq!(tui.title.as_deref(), Some("BOOTH"));
        // どちらにもないもの（タイムゾーンのずれは既定値、表示名は設定ファイル）
        assert_eq!(tui.footer, None);
        assert_eq!(
            tui.zone.zone(),
            DisplayZone::new(DisplayZone::jst().offset_secs(), "UTC")
        );

        let Command::Once(once) = merged(file, &["once"]) else {
            panic!("once のはず");
        };
        assert_eq!(once.target.server.as_deref(), Some("192.168.1.123"));
    }

    #[test]
    fn sections_apply_only_to_their_subcommand() {
        let file = "[tui]\ninterval-ms = 200\n\n[once]\njson = true\n";
        let Command::Stats(stats) = merged(file, &["stats", "127.0.0.1"]) else {
            panic!("stats のはず");
        };
        assert_eq!(
            polling_interval(stats.interval_ms),
            Duration::from_millis(500)
        );
        assert!(!stats.target.json);
    }

    #[test]
    fn flags_are_on_when_either_side_turns_them_on() {
        let file = "no-df = true\n\n[tui]\ntrend = true\n\n[stats]\njson = false\n";
        let Command::Tui(tui) = merged(file, &["tui", "127.0.0.1"]) else {
            panic!("tui のはず");
        };
        assert!(tui.trend);
        assert!(tui.transport.no_df);

        let Command::Stats(stats) = merged(file, &["stats", "127.0.0.1", "--json"]) else {
            panic!("stats のはず");
        };
        assert!(stats.target.json);
    }

    /// 設定ファイルのヘッダ指定も、コマンドラインの指定と同じく送信方式との組み合わせを確かめる。
    #[test]
    fn header_options_from_file_are_rejected_with_udp_from_command_line() {
        let file = "transport = \"raw-ip\"\ndscp = 46\n";
        let Command::Once(once) = merged(file, &["once", "127.0.0.1", "--transport", "udp"]) else {
            panic!("once のはず");
        };
        assert_eq!(once.transport.mode(), TransportMode::Udp);
        assert_eq!(
            once.transport
                .options()
                .headers
                .map(|headers| headers.ipv4.dscp),
            Some(46)
        );

        let clock: Arc<dyn ClockPort> = Arc::new(SystemClock::new());
        let error = once
            .transport
            .open(
                "127.0.0.1:123".parse().unwrap(),
                clock,
                Arc::new(TimestampingStatus::new()),
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("raw-ip / link"), "{error:#}");
    }

    #[test]
    fn server_must_come_from_command_line_or_file() {
        let Command::Stats(stats) = merged("", &["stats"]) else {
            panic!("stats のはず");
        };
        let error = required_server(stats.target.server.as_deref()).unwrap_err();
        assert!(error.to_string().contains("server"), "{error}");
    }

    /// 省略時の `config.toml` はなくてよいが、`--config` で指定したファイルがないのはエラー。
    #[test]
    fn only_explicit_config_file_must_exist() {
        let missing = Path::new("存在しないディレクトリ/config.toml");
        assert_eq!(load_config_if_exists(missing).unwrap().server, None);

        let error = load_config(missing).unwrap_err();
        assert!(
            format!("{error:#}").contains("存在しないディレクトリ"),
            "{error:#}"
        );
    }

    #[test]
    fn unknown_or_misplaced_keys_are_errors() {
        for (file, key) in [
            ("intervl-ms = 200\n", "intervl-ms"),
            ("[tui]\ntitel = \"x\"\n", "titel"),
            ("[unknown]\n", "unknown"),
            // 設定ファイルでは許さない
            ("allow-fast-poll = true\n", "allow-fast-poll"),
            ("[tui]\nallow-fast-poll = true\n", "allow-fast-poll"),
            // 共通の項目を節の後に書いた（その節のキーになる）
            ("[tui]\nserver = \"192.168.1.123\"\n", "server"),
        ] {
            let message = parse_error(file);
            assert!(message.contains(key), "{file}: {message}");
        }
    }

    #[test]
    fn values_are_checked_like_command_line() {
        for (file, expected) in [
            ("dscp = 64", "0〜63"),
            ("ecn = 4", "0〜3"),
            ("vlan = 4095", "0〜4094"),
            ("pcp = 8", "0〜7"),
            ("ttl = 256", "u8"),
            ("tz = \"+9\"", "+09:00 の形式"),
            ("udp-checksum = \"bad\"", "compute / zero"),
            ("ip-checksum = \"1234\"", "0x1234"),
            ("dst-mac = \"00:11:22\"", "aa:bb:cc:dd:ee:ff"),
            ("transport = \"raw_ip\"", "raw-ip"),
            ("[sim-server]\nrefid = \"TOOLONG\"", "4 文字以内"),
        ] {
            let message = parse_error(file);
            assert!(
                message.contains(expected) && message.contains("line"),
                "{file}: {message}"
            );
        }
    }

    #[test]
    fn enum_spellings_match_command_line() {
        for mode in ModeArg::value_variants() {
            let name = mode.to_possible_value().unwrap();
            let file = parse(&format!("transport = \"{}\"", name.get_name())).unwrap();
            assert_eq!(file.transport, Some(*mode));
        }
        for timestamping in TimestampingArg::value_variants() {
            let name = timestamping.to_possible_value().unwrap();
            let file = parse(&format!("timestamping = \"{}\"", name.get_name())).unwrap();
            assert_eq!(file.timestamping, Some(*timestamping));
        }
    }

    #[test]
    fn mib_spellings_match_command_line() {
        for mib in MibArg::value_variants() {
            let name = mib.to_possible_value().unwrap();
            let file = parse(&format!("[tui]\nsnmp-mib = \"{}\"", name.get_name())).unwrap();
            assert_eq!(file.tui.snmp_mib, Some(*mib));
        }
        for mib in SimMibArg::value_variants() {
            let name = mib.to_possible_value().unwrap();
            let file = parse(&format!("[sim-server]\nsnmp-mib = \"{}\"", name.get_name())).unwrap();
            assert_eq!(file.sim_server.snmp_mib, Some(*mib));
        }
        assert_eq!(
            MibArg::MicrochipS650
                .to_possible_value()
                .unwrap()
                .get_name(),
            "microchip-s650"
        );
    }

    #[test]
    fn snmp_settings_merge_and_point_at_the_ntp_server() {
        let file = "[tui]\nsnmp-community = \"public\"\nsnmp-mib = \"meinberg-ltng\"\n";
        let Command::Tui(tui) = merged(file, &["tui", "192.168.1.123", "--snmp-port", "16100"])
        else {
            panic!("tui のはず");
        };
        let (target, config) = tui
            .snmp
            .monitor("192.168.1.123:123".parse().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(target, "192.168.1.123:16100".parse().unwrap());
        assert_eq!(config.community, b"public");
        assert_eq!(config.profile, Some(MibProfile::MeinbergLtng));
        assert_eq!(config.interval, Duration::from_millis(5000));

        // 間隔は下限で止め、auto は判定する
        let Command::Tui(fast) = merged(
            "",
            &[
                "tui",
                "127.0.0.1",
                "--snmp-community",
                "x",
                "--snmp-interval-ms",
                "10",
                "--snmp-mib",
                "auto",
            ],
        ) else {
            panic!("tui のはず");
        };
        let (_, fast_config) = fast
            .snmp
            .monitor("127.0.0.1:123".parse().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(fast_config.interval, Duration::from_millis(1000));
        assert_eq!(fast_config.profile, None);
    }

    /// SNMP の指定は `snmp-community` がなければ使えない。コマンドラインでも設定ファイルでも黙って無視しない。
    #[test]
    fn snmp_settings_without_community_are_errors() {
        let server = "127.0.0.1:123".parse().unwrap();
        for (file, args) in [
            ("", &["tui", "127.0.0.1", "--snmp-port", "161"][..]),
            ("", &["tui", "127.0.0.1", "--snmp-mib", "auto"]),
            ("[tui]\nsnmp-interval-ms = 2000\n", &["tui", "127.0.0.1"]),
        ] {
            let Command::Tui(tui) = merged(file, args) else {
                panic!("tui のはず");
            };
            let error = tui.snmp.monitor(server).unwrap_err();
            assert!(error.to_string().contains("--snmp-community"), "{error}");
        }
        let Command::Tui(plain) = merged("", &["tui", "127.0.0.1"]) else {
            panic!("tui のはず");
        };
        assert!(plain.snmp.monitor(server).unwrap().is_none());
    }

    #[test]
    fn offset_may_be_written_as_integer() {
        let file = parse("[sim-server]\noffset-ms = -1\n").unwrap();
        assert_eq!(file.sim_server.offset_ms, Some(-1.0f64));
    }
}
