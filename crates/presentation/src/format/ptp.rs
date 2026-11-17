//! PTP の時計（ptp4l が合わせる PHC・スレーブとして問い合わせるマスタ）の表示用の整形（状態・ナノ秒の時間差・
//! 時刻の性質）。
//!
//! - PTP の時間差はナノ秒で出す。`µ`（マイクロ）は East Asian Ambiguous（端末の設定で幅が 1 にも 2 にもなる）
//!   なので使わず、0.1 ms 以上はミリ秒で出す
//! - PHC の状態の文言には ptp4l のポートの状態（`SLAVE` など）を添え、ptp4l の表示と見比べられるようにする
//! - 文言は CLI のもの。TUI の文言（英語）は `tui::text` にある（ナノ秒の整形はここのものを使う）

use core::net::SocketAddr;

use application::description::{PhcInfo, PhcReadMethod, PtpTransportInfo};
use application::snapshot::MasterSnapshot;
use domain::ptp_clock::{
    ClockRole, LeapWarning, MasterState, MasterStatus, PhcState, PtpStatus, TimeProperties,
    TimeSourceKind, Timescale,
};
use domain::shared_kernel::TimeDelta;

/// これより小さい時間差はナノ秒で出す（それ以上はミリ秒）
const NANOS_LIMIT: i64 = 100_000;

/// 符号付きの時間差: "+12 ns" / "-1234 ns" / "+0.250 ms"
#[must_use]
pub fn format_nanos_signed(delta: TimeDelta) -> String {
    if delta.abs().as_nanos() < NANOS_LIMIT {
        format!("{:+} ns", delta.as_nanos())
    } else {
        format!("{:+.3} ms", delta.as_millis_f64())
    }
}

/// 時間（経路の遅延など）: "1234 ns" / "0.250 ms"
#[must_use]
pub fn format_nanos(delta: TimeDelta) -> String {
    if delta.abs().as_nanos() < NANOS_LIMIT {
        format!("{} ns", delta.as_nanos())
    } else {
        format!("{:.3} ms", delta.as_millis_f64())
    }
}

/// PTP の送受信の説明: `nic0 192.168.10.200・UDP 319/320・マルチキャスト`・
/// `192.168.10.200・UDP 319/320・Delay_Req は 192.168.10.50:319 へ`
#[must_use]
pub fn ptp_transport_label(info: &PtpTransportInfo) -> String {
    let local = info.local;
    let name = info
        .interface
        .as_ref()
        .map_or_else(|| local.to_string(), |name| format!("{name} {local}"));
    let mode = info.unicast_to.map_or_else(
        || "マルチキャスト".to_owned(),
        |to| format!("Delay_Req は {to} へ"),
    );
    format!(
        "{name}・UDP {}/{}・{mode}",
        info.event_port,
        info.general_port()
    )
}

/// PHC の説明: "/dev/ptp0（同時読み取り）"・"/dev/ptp1（仮想 PHC・前後の読みで挟む）"
#[must_use]
pub fn phc_label(info: &PhcInfo) -> String {
    let mut notes = Vec::new();
    if info.virtual_clock {
        notes.push("仮想 PHC");
    }
    if info.from_interface {
        notes.push("インターフェースから特定");
    }
    notes.push(match info.method {
        PhcReadMethod::Precise => "同時読み取り",
        PhcReadMethod::Extended => "前後の読みで挟む",
        PhcReadMethod::Basic => "前後の読みで挟む・粗い",
    });
    format!("{}（{}）", info.path, notes.join("・"))
}

/// 状態の文言: "同期済み（SLAVE）"・"自走中（ptp4l の応答なし）" など。
/// ptp4l の状態がなければ、ポートの状態は添えない（"同期済み"）。
#[must_use]
pub fn phc_state_label(
    state: PhcState,
    status: Option<&PtpStatus>,
    status_error: Option<&str>,
) -> String {
    let with_ports = |base: &str| match status.map(port_states) {
        Some(ports) if !ports.is_empty() => format!("{base}（{ports}）"),
        _ => base.to_owned(),
    };
    match state {
        PhcState::Starting if status_error.is_some() => "ptp4l の応答なし".to_owned(),
        PhcState::Starting => "ptp4l の応答待ち".to_owned(),
        PhcState::Acquiring => match status.map(PtpStatus::role) {
            Some(ClockRole::Uncalibrated) => with_ports("マスタに合わせています"),
            _ => with_ports("マスタを探しています"),
        },
        PhcState::Synchronized => with_ports("同期済み"),
        PhcState::Master => with_ports("マスタとして動作中"),
        PhcState::Holdover if status_error.is_some() => "自走中（ptp4l の応答なし）".to_owned(),
        PhcState::Holdover => with_ports("自走中"),
        PhcState::Faulty => with_ports("ポートが使えません"),
    }
}

/// JSON 用の状態名。
#[must_use]
pub const fn phc_state_key(state: PhcState) -> &'static str {
    match state {
        PhcState::Starting => "starting",
        PhcState::Acquiring => "acquiring",
        PhcState::Synchronized => "synchronized",
        PhcState::Master => "master",
        PhcState::Holdover => "holdover",
        PhcState::Faulty => "faulty",
    }
}

/// ポートの状態を並べる: "SLAVE" / "MASTER・SLAVE"
fn port_states(status: &PtpStatus) -> String {
    status
        .ports
        .iter()
        .map(|port| port.state.to_string())
        .collect::<Vec<_>>()
        .join("・")
}

/// ポートごとのインターフェースと状態: "eno1 SLAVE" / "eth0 MASTER、eth1 SLAVE"。
/// `interfaces` はポート番号とインターフェース名の組。
#[must_use]
pub fn port_label(status: &PtpStatus, interfaces: &[(u16, String)]) -> String {
    status
        .ports
        .iter()
        .map(|port| {
            interfaces
                .iter()
                .find(|pair| pair.0 == port.number)
                .map_or_else(
                    || format!("ポート {} {}", port.number, port.state),
                    |pair| format!("{} {}", pair.1, port.state),
                )
        })
        .collect::<Vec<_>>()
        .join("、")
}

/// clockClass: "6（基準に同期）" / "248（既定）"。意味がよく使われるものだけ添える。
#[must_use]
pub fn clock_class_label(class: u8) -> String {
    let meaning = match class {
        6 => "基準に同期",
        7 => "基準を失い保持中",
        13 => "ARB の基準に同期",
        14 => "ARB の基準を失い保持中",
        52 | 58 | 187 | 193 => "保持の期限切れ",
        248 => "既定",
        255 => "スレーブ専用",
        _ => return class.to_string(),
    };
    format!("{class}（{meaning}）")
}

/// timeSource の名前。
#[must_use]
pub fn time_source_label(kind: TimeSourceKind) -> String {
    let name = match kind {
        TimeSourceKind::AtomicClock => "原子時計",
        TimeSourceKind::Gnss => "GNSS",
        TimeSourceKind::TerrestrialRadio => "地上の電波",
        TimeSourceKind::SerialTimeCode => "シリアルのタイムコード",
        TimeSourceKind::Ptp => "PTP",
        TimeSourceKind::Ntp => "NTP",
        TimeSourceKind::HandSet => "手動で設定",
        TimeSourceKind::Other => "その他",
        TimeSourceKind::InternalOscillator => "内部の発振器",
        TimeSourceKind::Unknown(bits) => return format!("不明（0x{bits:02X}）"),
    };
    name.to_owned()
}

/// JSON 用の timeSource の名前。
#[must_use]
pub fn time_source_key(kind: TimeSourceKind) -> String {
    let name = match kind {
        TimeSourceKind::AtomicClock => "atomic_clock",
        TimeSourceKind::Gnss => "gnss",
        TimeSourceKind::TerrestrialRadio => "terrestrial_radio",
        TimeSourceKind::SerialTimeCode => "serial_time_code",
        TimeSourceKind::Ptp => "ptp",
        TimeSourceKind::Ntp => "ntp",
        TimeSourceKind::HandSet => "hand_set",
        TimeSourceKind::Other => "other",
        TimeSourceKind::InternalOscillator => "internal_oscillator",
        TimeSourceKind::Unknown(bits) => return format!("0x{bits:02x}"),
    };
    name.to_owned()
}

/// UTC との差: "37 秒" / "37 秒（未確定）" / "換算なし（ARB）"。2 つ目は、確かな値で換算しているか。
#[must_use]
pub fn utc_offset_label(time: &TimeProperties) -> (String, bool) {
    match time.timescale {
        Timescale::Arbitrary => ("換算なし（時刻系が ARB）".to_owned(), false),
        Timescale::Ptp if time.utc_offset_valid => (format!("{} 秒", time.utc_offset), true),
        Timescale::Ptp => (format!("{} 秒（未確定）", time.utc_offset), false),
    }
}

/// うるう秒の予告。
#[must_use]
pub const fn ptp_leap_label(leap: LeapWarning) -> &'static str {
    match leap {
        LeapWarning::None => "なし",
        LeapWarning::Insert => "うるう秒の挿入予告",
        LeapWarning::Delete => "うるう秒の削除予告",
    }
}

/// JSON 用のうるう秒予告の名前。
#[must_use]
pub const fn ptp_leap_key(leap: LeapWarning) -> &'static str {
    match leap {
        LeapWarning::None => "none",
        LeapWarning::Insert => "insert",
        LeapWarning::Delete => "delete",
    }
}

/// PTP のマスタの状態の文言: "同期済み"・"自走中（マスタを見失いました）" など。
/// 測れていない理由（直近の交換の失敗）があれば添える。
#[must_use]
pub fn master_state_label(snapshot: &MasterSnapshot) -> String {
    let with_failure = |base: &str| {
        snapshot
            .last_failure
            .as_ref()
            .map_or_else(|| base.to_owned(), |failure| format!("{base}（{failure}）"))
    };
    match snapshot.state {
        MasterState::Listening if snapshot.receive_error.is_some() => "受信できません".to_owned(),
        MasterState::Listening => format!("マスタを探しています（ドメイン {}）", snapshot.domain),
        MasterState::Uncalibrated => with_failure("マスタの時刻を測っています"),
        MasterState::Synchronized => "同期済み".to_owned(),
        MasterState::Holdover if snapshot.status.is_none() => {
            "自走中（マスタを見失いました）".to_owned()
        }
        MasterState::Holdover => with_failure("自走中"),
    }
}

/// JSON 用の、PTP のマスタの状態名。
#[must_use]
pub const fn master_state_key(state: MasterState) -> &'static str {
    match state {
        MasterState::Listening => "listening",
        MasterState::Uncalibrated => "uncalibrated",
        MasterState::Synchronized => "synchronized",
        MasterState::Holdover => "holdover",
    }
}

/// マスタ: "001b21.fffe.123456-1（192.168.10.50）"
#[must_use]
pub fn master_label(status: &MasterStatus, address: Option<SocketAddr>) -> String {
    address.map_or_else(
        || status.master.to_string(),
        |from| format!("{}（{}）", status.master, from.ip()),
    )
}

/// 間隔: "1 秒" / "0.125 秒" / "2 秒"
#[must_use]
pub fn format_interval(delta: TimeDelta) -> String {
    format!("{} 秒", format_seconds(delta))
}

/// 秒の数だけ（ミリ秒まで。末尾の 0 は省く）: "1" / "0.125"
#[must_use]
pub fn format_seconds(delta: TimeDelta) -> String {
    let text = format!("{:.3}", delta.as_secs_f64());
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// Sync と `Delay_Req` の間隔: "Sync 1 秒（two-step）・Delay_Req 1 秒"。
/// `Delay_Req` がマスタの最小間隔のために利用者の指定より長ければ、その旨を添える。
#[must_use]
pub fn intervals_label(snapshot: &MasterSnapshot) -> String {
    let status = snapshot.status;
    let sync = status
        .and_then(|master| master.sync_interval)
        .map_or_else(|| "--".to_owned(), format_interval);
    let step = match status.and_then(|master| master.two_step) {
        Some(true) => "（two-step）",
        Some(false) => "（one-step）",
        None => "",
    };
    // マスタが最小間隔を知らせていなければ、既定のプロファイルの既定値（1 秒）とみなしている
    let limited = match snapshot.master_min_interval {
        _ if snapshot.request_interval <= snapshot.configured_interval => "",
        Some(_) => "（マスタの最小間隔）",
        None => "（既定の最小間隔）",
    };
    format!(
        "Sync {sync}{step}・Delay_Req {}{limited}",
        format_interval(snapshot.request_interval)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::net::Ipv4Addr;
    use domain::ptp_clock::{ClockIdentity, ClockQuality, PortState, PortStatus};

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
    fn nanoseconds_switch_to_milliseconds_at_a_tenth_of_a_millisecond() {
        assert_eq!(format_nanos_signed(TimeDelta::from_nanos(-12)), "-12 ns");
        assert_eq!(
            format_nanos_signed(TimeDelta::from_nanos(99_999)),
            "+99999 ns"
        );
        assert_eq!(
            format_nanos_signed(TimeDelta::from_micros(250)),
            "+0.250 ms"
        );
        assert_eq!(format_nanos(TimeDelta::from_nanos(1_234)), "1234 ns");
        assert_eq!(format_nanos(TimeDelta::from_millis(3)), "3.000 ms");
    }

    #[test]
    fn states_name_the_ports() {
        let slave = status(&[PortState::Master, PortState::Slave]);
        assert_eq!(
            phc_state_label(PhcState::Synchronized, Some(&slave), None),
            "同期済み（MASTER・SLAVE）"
        );
        let listening = status(&[PortState::Listening]);
        assert_eq!(
            phc_state_label(PhcState::Acquiring, Some(&listening), None),
            "マスタを探しています（LISTENING）"
        );
        assert_eq!(
            phc_state_label(PhcState::Holdover, Some(&slave), Some("応答なし")),
            "自走中（ptp4l の応答なし）"
        );
        assert_eq!(
            phc_state_label(PhcState::Starting, None, Some("応答なし")),
            "ptp4l の応答なし"
        );
        assert_eq!(
            phc_state_label(PhcState::Acquiring, None, None),
            "マスタを探しています"
        );
        assert_eq!(
            port_label(&slave, &[(1, "eth0".to_owned()), (2, "eth1".to_owned())]),
            "eth0 MASTER、eth1 SLAVE"
        );
    }

    #[test]
    fn transports_and_phcs_are_described() {
        let local = Ipv4Addr::new(192, 168, 10, 200);
        let mut transport = PtpTransportInfo {
            interface: Some("nic0".to_owned()),
            local,
            event_port: 319,
            unicast_to: None,
        };
        assert_eq!(
            ptp_transport_label(&transport),
            "nic0 192.168.10.200・UDP 319/320・マルチキャスト"
        );
        transport.interface = None;
        transport.unicast_to = Some(SocketAddr::from(([192, 168, 10, 50], 319)));
        assert_eq!(
            ptp_transport_label(&transport),
            "192.168.10.200・UDP 319/320・Delay_Req は 192.168.10.50:319 へ"
        );
        let mut phc = PhcInfo {
            path: "/dev/ptp0".to_owned(),
            virtual_clock: false,
            from_interface: false,
            method: PhcReadMethod::Precise,
        };
        assert_eq!(phc_label(&phc), "/dev/ptp0（同時読み取り）");
        phc.virtual_clock = true;
        phc.from_interface = true;
        phc.method = PhcReadMethod::Basic;
        assert_eq!(
            phc_label(&phc),
            "/dev/ptp0（仮想 PHC・インターフェースから特定・前後の読みで挟む・粗い）"
        );
    }

    #[test]
    fn utc_offset_is_marked_when_not_trustworthy() {
        let mut time = status(&[]).time;
        assert_eq!(utc_offset_label(&time), ("37 秒".to_owned(), true));
        time.utc_offset_valid = false;
        assert_eq!(
            utc_offset_label(&time),
            ("37 秒（未確定）".to_owned(), false)
        );
        time.timescale = Timescale::Arbitrary;
        assert!(!utc_offset_label(&time).1);
        assert_eq!(clock_class_label(6), "6（基準に同期）");
        assert_eq!(clock_class_label(135), "135");
    }
}
