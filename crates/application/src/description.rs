//! 開いた送受信の口と PHC の説明（読み取り用）。
//!
//! 送受信・PHC（infrastructure）が「何を開いたか」の事実を埋め、表示（presentation）が文言にする。
//! 文言を下の層で作らないのは、CLI（日本語）と TUI（英語）で言葉が違うため。

use core::net::{Ipv4Addr, SocketAddr};

use domain::packet_crafting::MacAddr;

/// NTP の送受信の口。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NtpTransportInfo {
    pub kind: NtpTransportKind,
    /// NIC のハードウェアのタイムスタンプを使うときの NIC。使わなければ `None`
    pub hardware: Option<HardwareTimestampInfo>,
}

/// NTP の送信方式。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NtpTransportKind {
    /// ふつうの UDP ソケット
    Udp,
    /// IP / UDP ヘッダを自作する
    RawIp,
    /// Ethernet フレームを自作する
    Link {
        /// 送るインターフェースの名前
        interface: String,
        /// 宛先 MAC（指定したもの、または近隣表・ARP で調べたもの）
        destination: MacAddr,
    },
}

/// ハードウェアのタイムスタンプに使う NIC。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HardwareTimestampInfo {
    /// NIC の名前（"nic0"）
    pub device: String,
    /// NIC で無効だったので、こちらで有効にしたか（終了時に戻す）
    pub enabled_temporarily: bool,
}

/// PTP の送受信の口。タイムスタンプの方式は含めない（実際に取れた方式は取得状況として別に出す）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PtpTransportInfo {
    /// 名前で指定したインターフェース（アドレスで指定したとき・省いたときは `None`）
    pub interface: Option<String>,
    /// 使う自分のアドレス
    pub local: Ipv4Addr,
    /// イベント用のポート（一般用はその次の番号）
    pub event_port: u16,
    /// `Delay_Req` をユニキャストで送る宛先。マルチキャストなら `None`
    pub unicast_to: Option<SocketAddr>,
}

impl PtpTransportInfo {
    /// 一般用のポート（イベント用の次の番号）。
    #[must_use]
    pub const fn general_port(&self) -> u16 {
        self.event_port.saturating_add(1)
    }
}

/// 開いた PHC。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhcInfo {
    /// "/dev/ptp0"
    pub path: String,
    /// 仮想 PHC（vclock）か
    pub virtual_clock: bool,
    /// ptp4l が PHC の番号を知らせず、ポートのインターフェースの PHC を使ったか（古い linuxptp）
    pub from_interface: bool,
    pub method: PhcReadMethod,
}

/// PHC とシステムの時計の読み方（精度の高い順）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhcReadMethod {
    /// NIC とシステムの時計を同時に読む（対応している NIC だけ）
    Precise,
    /// システムの時計で PHC の読みを挟み、幅が最小の標本を使う
    Extended,
    /// `Extended` と同じだが、挟む時刻がドライバの外（幅が広い）
    Basic,
}
