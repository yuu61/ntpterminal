//! TUI の文言（英語）。
//!
//! - TUI の文言は英語で書く。CLI（`once`・`stats`）の文言は日本語のまま [`crate::format`] にある。同じことを
//!   知らせるものは、どの文言にするかの判断を CLI と同じにし（[`TimestampingSummary`] など）、言葉だけを替える
//! - 使う文字は ASCII だけ。全角の括弧・中点（`（）・、`）や East Asian Ambiguous の文字（`→`・`−`・`µ` など。
//!   端末の設定で幅が 1 にも 2 にもなる）は使わない。区切りは `, ` と ` / `、括弧は `()`。例外は状態の印 `●`
//!   （`screen`）と、大きい時計・推移の棒を描く文字（`big_digits`・`trend` の先頭）
//! - 下の層の値（送受信の口・PHC・交換の失敗・SNMP のエラー）も、文言ではなく種類を受け取ってここ（SNMP の
//!   エラーは `receiver`）で言葉にする。OS が返すエラーの説明（`io::Error` の文言）だけはそのまま出す
//!   （Windows では OS の表示言語になる）

use core::net::SocketAddr;

use application::description::{
    NtpTransportInfo, NtpTransportKind, PhcInfo, PhcReadMethod, PtpTransportInfo,
};
use application::snapshot::MasterSnapshot;
use application::timestamping::TimestampingCounts;
use domain::ptp_clock::{
    ClockRole, ExchangeFailure, LeapWarning, MasterState, MasterStatus, PhcState, PortState,
    PtpStatus, TimeProperties, TimeSourceKind, Timescale,
};
use domain::shared_kernel::{NtpTimestamp, TimeDelta};
use domain::time_sync::{SampleError, SyncState};

use crate::cli::master::MasterTarget;
use crate::cli::phc::PhcTarget;
use crate::format::ptp::format_seconds;
use crate::format::{DisplayZone, TimestampingSummary, to_civil};

/// 曜日（日曜始まり）。
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// 日付だけ: "2026-11-15 (Sun)"
pub(super) fn date(timestamp: NtpTimestamp, zone: &DisplayZone) -> String {
    let civil = to_civil(timestamp, zone.offset_secs());
    format!(
        "{:04}-{:02}-{:02} ({})",
        civil.year, civil.month, civil.day, WEEKDAYS[civil.weekday as usize]
    )
}

/// 経過時間: "7.0 s ago"
pub(super) fn ago(elapsed: TimeDelta) -> String {
    format!("{:.1} s ago", elapsed.max(TimeDelta::ZERO).as_secs_f64())
}

/// 残り時間: "in 9.0 s"
pub(super) fn after(remaining: TimeDelta) -> String {
    format!("in {:.1} s", remaining.max(TimeDelta::ZERO).as_secs_f64())
}

/// NTP の状態の行。
pub(super) const fn ntp_state(state: SyncState) -> &'static str {
    match state {
        SyncState::Unsynchronized => "Waiting for reply (unsynchronized)",
        SyncState::Synchronized => "Synchronized",
        SyncState::Holdover => "Holdover (no reply)",
        SyncState::Denied => "Denied by server (KoD)",
    }
}

/// タイムスタンプの出どころ（直近の受信）。設定した出どころの時刻が取れていないことを隠さない。
pub(super) const fn timestamping(counts: &TimestampingCounts) -> &'static str {
    match TimestampingSummary::of(counts) {
        TimestampingSummary::User => "User space",
        TimestampingSummary::HardwareWaiting => "Hardware (waiting for reply)",
        TimestampingSummary::KernelWaiting => "Kernel (waiting for reply)",
        TimestampingSummary::Hardware => "Hardware",
        TimestampingSummary::KernelInsteadOfHardware => "Kernel (no hardware timestamp)",
        TimestampingSummary::Kernel => "Kernel",
        TimestampingSummary::UserInsteadOfKernel => "User space (no kernel timestamp)",
    }
}

/// NTP の送信方式: "UDP socket"・"link (eth0 -> 00:11:22:33:44:55), eth0 HW clock (temp. enabled)"。
/// link とハードウェアのタイムスタンプを合わせても、ラベルと合わせて 80 桁に収まる長さにする
#[expect(
    clippy::pattern_type_mismatch,
    reason = "インターフェースの名前（String）を借りるには、参照のまま照合するしかない（ref は ref_patterns で禁止）"
)]
pub(super) fn ntp_transport(info: &NtpTransportInfo) -> String {
    let base = match &info.kind {
        NtpTransportKind::Udp => "UDP socket".to_owned(),
        NtpTransportKind::RawIp => "raw-ip (custom IP/UDP headers)".to_owned(),
        NtpTransportKind::Link {
            interface,
            destination,
        } => format!("link ({interface} -> {destination})"),
    };
    match info.hardware.as_ref() {
        None => base,
        Some(nic) if nic.enabled_temporarily => {
            format!("{base}, {} HW clock (temp. enabled)", nic.device)
        }
        Some(nic) => format!("{base}, {} HW clock", nic.device),
    }
}

/// PTP の送受信: `nic0 192.168.10.200, UDP 319/320, multicast`・
/// `192.168.10.200, UDP 319/320, Delay_Req to 192.168.10.50:319`
pub(super) fn ptp_transport(info: &PtpTransportInfo) -> String {
    let local = info.local;
    let name = info
        .interface
        .as_ref()
        .map_or_else(|| local.to_string(), |name| format!("{name} {local}"));
    let mode = info
        .unicast_to
        .map_or_else(|| "multicast".to_owned(), |to| format!("Delay_Req to {to}"));
    format!(
        "{name}, UDP {}/{}, {mode}",
        info.event_port,
        info.general_port()
    )
}

/// PHC: "/dev/ptp0 (simultaneous read)"・"/dev/ptp1 (virtual PHC, bracketed read)"
pub(super) fn phc(info: &PhcInfo) -> String {
    let mut notes = Vec::new();
    if info.virtual_clock {
        notes.push("virtual PHC");
    }
    if info.from_interface {
        notes.push("found via interface");
    }
    notes.push(match info.method {
        PhcReadMethod::Precise => "simultaneous read",
        PhcReadMethod::Extended => "bracketed read",
        PhcReadMethod::Basic => "coarse bracketed read",
    });
    format!("{} ({})", info.path, notes.join(", "))
}

/// 見出しの下に出す名前（ptp4l が合わせる PHC）: "/var/run/ptp4l (domain 0)"
#[must_use]
pub fn phc_server_label(target: &PhcTarget) -> String {
    format!("{} (domain {})", target.socket, target.domain)
}

/// 見出しの下に出す名前（PTP のマスタ）: "PTP master (domain 0)"
#[must_use]
pub fn master_server_label(target: &MasterTarget) -> String {
    format!("PTP master (domain {})", target.domain)
}

/// PHC の状態の行: "Synchronized (SLAVE)"・"Holdover (no response from ptp4l)" など。
/// どの文言にするかは CLI の [`crate::format::ptp::phc_state_label`] と同じ。`ptp4l_silent` は、直近の ptp4l への
/// 問い合わせが失敗したか。
pub(super) fn phc_state(state: PhcState, status: Option<&PtpStatus>, ptp4l_silent: bool) -> String {
    let with_ports = |base: &str| match status.map(port_states) {
        Some(ports) if !ports.is_empty() => format!("{base} ({ports})"),
        _ => base.to_owned(),
    };
    match state {
        PhcState::Starting if ptp4l_silent => "No response from ptp4l".to_owned(),
        PhcState::Starting => "Waiting for ptp4l".to_owned(),
        PhcState::Acquiring => match status.map(PtpStatus::role) {
            Some(ClockRole::Uncalibrated) => with_ports("Locking to master"),
            _ => with_ports("Looking for master"),
        },
        PhcState::Synchronized => with_ports("Synchronized"),
        PhcState::Master => with_ports("Acting as master"),
        PhcState::Holdover if ptp4l_silent => "Holdover (no response from ptp4l)".to_owned(),
        PhcState::Holdover => with_ports("Holdover"),
        PhcState::Faulty => with_ports("Port faulty"),
    }
}

/// ポートの状態（ptp4l の表示と同じ "SLAVE" など）。知らない値は "UNKNOWN(n)"（`Display` は CLI 用の日本語）。
fn port_state(state: PortState) -> String {
    if let PortState::Unknown(bits) = state {
        format!("UNKNOWN({bits})")
    } else {
        state.to_string()
    }
}

/// ポートの状態を並べる: "SLAVE" / "MASTER, SLAVE"
fn port_states(status: &PtpStatus) -> String {
    status
        .ports
        .iter()
        .map(|port| port_state(port.state))
        .collect::<Vec<_>>()
        .join(", ")
}

/// ポートごとのインターフェースと状態: "eno1 SLAVE" / "eth0 MASTER, eth1 SLAVE"。
/// `interfaces` はポート番号とインターフェース名の組。
pub(super) fn ports(status: &PtpStatus, interfaces: &[(u16, String)]) -> String {
    status
        .ports
        .iter()
        .map(|port| {
            let state = port_state(port.state);
            interfaces
                .iter()
                .find(|pair| pair.0 == port.number)
                .map_or_else(
                    || format!("port {} {state}", port.number),
                    |pair| format!("{} {state}", pair.1),
                )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// PTP のマスタの状態の行: "Synchronized"・"Holdover (master lost)" など。
/// どの文言にするかは CLI の [`crate::format::ptp::master_state_label`] と同じ。
pub(super) fn master_state(snapshot: &MasterSnapshot) -> String {
    let with_failure = |base: &str| {
        snapshot.last_failure.as_ref().map_or_else(
            || base.to_owned(),
            |failure| format!("{base} ({})", exchange_failure(failure)),
        )
    };
    match snapshot.state {
        MasterState::Listening if snapshot.receive_error.is_some() => "Cannot receive".to_owned(),
        MasterState::Listening => format!("Looking for master (domain {})", snapshot.domain),
        MasterState::Uncalibrated => with_failure("Measuring master time"),
        MasterState::Synchronized => "Synchronized".to_owned(),
        MasterState::Holdover if snapshot.status.is_none() => "Holdover (master lost)".to_owned(),
        MasterState::Holdover => with_failure("Holdover"),
    }
}

/// 交換が失敗した理由。`Delay_Req` を送れなかったときの送受信の口のエラーの説明は出さない
/// （CLI の文言で作られていて英語ではないため。`stats --ptp` の出力で見られる）。
const fn exchange_failure(failure: &ExchangeFailure) -> &'static str {
    match *failure {
        ExchangeFailure::RequestNotSent => "Delay_Req not sent",
        ExchangeFailure::FollowUpMissing => "no Follow_Up",
        ExchangeFailure::DelayRespMissing => "no Delay_Resp",
        ExchangeFailure::SendFailed(_) => "cannot send Delay_Req",
        ExchangeFailure::InvalidSample(SampleError::TimebaseMismatch(_)) => "timebase mismatch",
        ExchangeFailure::InvalidSample(SampleError::ServerTimeReversed) => "remote time reversed",
        ExchangeFailure::InvalidSample(SampleError::LocalTimeReversed) => "local time reversed",
    }
}

/// マスタ: "001b21.fffe.123456-1 (192.168.10.50)"
pub(super) fn master(status: &MasterStatus, address: Option<SocketAddr>) -> String {
    address.map_or_else(
        || status.master.to_string(),
        |from| format!("{} ({})", status.master, from.ip()),
    )
}

/// clockClass: "6 (synced to reference)" / "248 (default)"。意味がよく使われるものだけ添える。
pub(super) fn clock_class(class: u8) -> String {
    let meaning = match class {
        6 => "synced to reference",
        7 => "reference lost, holdover",
        13 => "synced to ARB reference",
        14 => "ARB reference lost, holdover",
        52 | 58 | 187 | 193 => "holdover expired",
        248 => "default",
        255 => "slave-only",
        _ => return class.to_string(),
    };
    format!("{class} ({meaning})")
}

/// timeSource の名前。
pub(super) fn time_source(kind: TimeSourceKind) -> String {
    let name = match kind {
        TimeSourceKind::AtomicClock => "Atomic clock",
        TimeSourceKind::Gnss => "GNSS",
        TimeSourceKind::TerrestrialRadio => "Terrestrial radio",
        TimeSourceKind::SerialTimeCode => "Serial time code",
        TimeSourceKind::Ptp => "PTP",
        TimeSourceKind::Ntp => "NTP",
        TimeSourceKind::HandSet => "Hand set",
        TimeSourceKind::Other => "Other",
        TimeSourceKind::InternalOscillator => "Internal oscillator",
        TimeSourceKind::Unknown(bits) => return format!("Unknown (0x{bits:02X})"),
    };
    name.to_owned()
}

/// UTC との差: "37 s" / "37 s (unconfirmed)" / "n/a (ARB timescale)"。2 つ目は、確かな値で換算しているか。
pub(super) fn utc_offset(time: &TimeProperties) -> (String, bool) {
    match time.timescale {
        Timescale::Arbitrary => ("n/a (ARB timescale)".to_owned(), false),
        Timescale::Ptp if time.utc_offset_valid => (format!("{} s", time.utc_offset), true),
        Timescale::Ptp => (format!("{} s (unconfirmed)", time.utc_offset), false),
    }
}

/// うるう秒の予告。
pub(super) const fn leap(warning: LeapWarning) -> &'static str {
    match warning {
        LeapWarning::None => "None",
        LeapWarning::Insert => "Insertion announced",
        LeapWarning::Delete => "Deletion announced",
    }
}

/// Sync と `Delay_Req` の間隔: `Sync 1 s (two-step), Delay_Req 1 s`。
/// `Delay_Req` がマスタの最小間隔のために利用者の指定より長ければ、その旨を添える。
pub(super) fn intervals(snapshot: &MasterSnapshot) -> String {
    let status = snapshot.status;
    let sync = status
        .and_then(|master| master.sync_interval)
        .map_or_else(|| "--".to_owned(), seconds);
    let step = match status.and_then(|master| master.two_step) {
        Some(true) => " (two-step)",
        Some(false) => " (one-step)",
        None => "",
    };
    // マスタが最小間隔を知らせていなければ、既定のプロファイルの既定値（1 秒）とみなしている
    let limited = match snapshot.master_min_interval {
        _ if snapshot.request_interval <= snapshot.configured_interval => "",
        Some(_) => " (master minimum)",
        None => " (default minimum)",
    };
    format!(
        "Sync {sync}{step}, Delay_Req {}{limited}",
        seconds(snapshot.request_interval)
    )
}

/// 間隔: "1 s" / "0.125 s"
fn seconds(delta: TimeDelta) -> String {
    format!("{} s", format_seconds(delta))
}

#[cfg(test)]
mod tests {
    use super::*;
    use application::description::HardwareTimestampInfo;
    use application::timestamping::TimestampSource;
    use core::net::Ipv4Addr;
    use domain::packet_crafting::MacAddr;
    use domain::ptp_clock::{ClockIdentity, ClockQuality, PortStatus};

    #[test]
    fn dates_use_english_weekdays() {
        // 2026-11-15 08:25:45 UTC = 17:25:45 JST（日曜）
        let unix_secs = 20_772i128 * 86_400 + 8 * 3600 + 25 * 60 + 45;
        let timestamp = NtpTimestamp::from_unix_nanos(unix_secs * 1_000_000_000);
        assert_eq!(date(timestamp, &DisplayZone::jst()), "2026-11-15 (Sun)");
        assert_eq!(ago(TimeDelta::from_millis(7_049)), "7.0 s ago");
        assert_eq!(after(TimeDelta::from_millis(-5)), "in 0.0 s");
    }

    #[test]
    fn transports_keep_the_facts() {
        let link = NtpTransportInfo {
            kind: NtpTransportKind::Link {
                interface: "eth0".to_owned(),
                destination: MacAddr([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]),
            },
            hardware: Some(HardwareTimestampInfo {
                device: "eth0".to_owned(),
                enabled_temporarily: true,
            }),
        };
        assert_eq!(
            ntp_transport(&link),
            "link (eth0 -> 00:11:22:33:44:55), eth0 HW clock (temp. enabled)"
        );
        let udp = NtpTransportInfo {
            kind: NtpTransportKind::Udp,
            hardware: None,
        };
        assert_eq!(ntp_transport(&udp), "UDP socket");

        let mut ptp = PtpTransportInfo {
            interface: Some("nic0".to_owned()),
            local: Ipv4Addr::new(192, 168, 10, 200),
            event_port: 319,
            unicast_to: None,
        };
        assert_eq!(
            ptp_transport(&ptp),
            "nic0 192.168.10.200, UDP 319/320, multicast"
        );
        ptp.interface = None;
        ptp.unicast_to = Some(SocketAddr::from(([192, 168, 10, 50], 319)));
        assert_eq!(
            ptp_transport(&ptp),
            "192.168.10.200, UDP 319/320, Delay_Req to 192.168.10.50:319"
        );

        let info = PhcInfo {
            path: "/dev/ptp1".to_owned(),
            virtual_clock: true,
            from_interface: false,
            method: PhcReadMethod::Basic,
        };
        assert_eq!(phc(&info), "/dev/ptp1 (virtual PHC, coarse bracketed read)");
    }

    fn status(states: &[PortState]) -> PtpStatus {
        PtpStatus {
            domain: 0,
            ports: states
                .iter()
                .zip(1u16..)
                .map(|(&state, number)| PortStatus { number, state })
                .collect(),
            offset_from_master: TimeDelta::ZERO,
            mean_path_delay: TimeDelta::ZERO,
            steps_removed: 1,
            grandmaster: ClockIdentity::default(),
            grandmaster_quality: ClockQuality::default(),
            time: TimeProperties {
                utc_offset: 37,
                utc_offset_valid: true,
                timescale: Timescale::Ptp,
                leap: LeapWarning::None,
                time_traceable: true,
                frequency_traceable: true,
                time_source: TimeSourceKind::Gnss,
            },
        }
    }

    #[test]
    fn phc_states_name_the_ports() {
        let ports_of = status(&[PortState::Master, PortState::Unknown(42)]);
        assert_eq!(
            phc_state(PhcState::Synchronized, Some(&ports_of), false),
            "Synchronized (MASTER, UNKNOWN(42))"
        );
        assert_eq!(
            phc_state(PhcState::Holdover, Some(&ports_of), true),
            "Holdover (no response from ptp4l)"
        );
        assert_eq!(
            phc_state(PhcState::Starting, None, true),
            "No response from ptp4l"
        );
        assert_eq!(
            ports(&ports_of, &[(1, "eth0".to_owned())]),
            "eth0 MASTER, port 2 UNKNOWN(42)"
        );
    }

    #[test]
    fn time_properties_are_marked_when_not_trustworthy() {
        let mut time = status(&[]).time;
        assert_eq!(utc_offset(&time), ("37 s".to_owned(), true));
        time.utc_offset_valid = false;
        assert_eq!(utc_offset(&time), ("37 s (unconfirmed)".to_owned(), false));
        time.timescale = Timescale::Arbitrary;
        assert_eq!(utc_offset(&time), ("n/a (ARB timescale)".to_owned(), false));
        assert_eq!(clock_class(6), "6 (synced to reference)");
        assert_eq!(clock_class(135), "135");
        assert_eq!(time_source(TimeSourceKind::Unknown(0x42)), "Unknown (0x42)");
    }

    /// 設定した出どころの時刻が取れていないことを、英語の文言でも隠さない。
    #[test]
    fn timestamping_does_not_hide_fallbacks() {
        assert_eq!(timestamping(&TimestampingCounts::default()), "User space");
        let kernel = TimestampingCounts {
            kernel_enabled: true,
            ..TimestampingCounts::default()
        };
        assert_eq!(timestamping(&kernel), "Kernel (waiting for reply)");
        let hardware = TimestampingCounts {
            hardware_enabled: true,
            ..kernel
        };
        assert_eq!(timestamping(&hardware), "Hardware (waiting for reply)");
        let hardware_ok = TimestampingCounts {
            hardware_samples: 1,
            last_source: TimestampSource::Hardware,
            ..hardware
        };
        assert_eq!(timestamping(&hardware_ok), "Hardware");
        let hardware_fell_back = TimestampingCounts {
            kernel_samples: 1,
            last_source: TimestampSource::Kernel,
            ..hardware_ok
        };
        assert_eq!(
            timestamping(&hardware_fell_back),
            "Kernel (no hardware timestamp)"
        );
        let user_fallback = TimestampingCounts {
            user_samples: 1,
            last_source: TimestampSource::User,
            ..kernel
        };
        assert_eq!(
            timestamping(&user_fallback),
            "User space (no kernel timestamp)"
        );
    }

    #[test]
    fn exchange_failures_do_not_pass_through_other_wording() {
        assert_eq!(
            exchange_failure(&ExchangeFailure::SendFailed("通信エラー: x".to_owned())),
            "cannot send Delay_Req"
        );
        assert_eq!(
            exchange_failure(&ExchangeFailure::DelayRespMissing),
            "no Delay_Resp"
        );
    }
}
