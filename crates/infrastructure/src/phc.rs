//! NIC の時計（PHC、`/dev/ptpN`）の読み取り（Linux）。
//!
//! - PHC はシステムの時計（`CLOCK_REALTIME`）と組にして読む。NIC とシステムの時計を同時に読める
//!   `PTP_SYS_OFFSET_PRECISE`（対応している NIC だけ）を先に試し、使えなければ `PTP_SYS_OFFSET_EXTENDED`
//!   （システムの時計で PHC の読みを前後から挟む。前後の幅が最小の標本を使う）。それも使えなければ
//!   `PTP_SYS_OFFSET`（挟み方は同じだが、挟む時刻はドライバの外なので幅が広い。どの PHC でも使える。
//!   Hyper-V の PHC は PRECISE も EXTENDED も使えなかった）。`PTP_SYS_OFFSET` は PHC の表示（[`PhcDevice`]。
//!   読み方を画面に出す）だけで使い、NTP のハードウェアのタイムスタンプでは使わない（従来どおりエラーにする）
//! - REALTIME の時刻は、読んだ直後の PC の時刻を使って [`rebase_kernel_time`] でモノトニック時計に写す
//!   （カーネルのタイムスタンプと同じ道。ドメインにはモノトニック時計の時刻だけを渡す）
//! - `/dev/ptpN` を開くには、その権限（ふつうは root）が要る
//!
//! 使うのは、NTP のハードウェアのタイムスタンプ（PHC の時刻を REALTIME に直す）と、
//! ptp4l が合わせる PHC の表示（[`PhcDevice`]）。

use alloc::sync::Arc;
use core::ptr;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd as _;

use application::port::{ClockPort, PhcPort, PhcReadError};
use domain::ptp_clock::{PhcReading, PhcTime};
use nix::libc;

use crate::clock::rebase_kernel_time;

/// `PTP_SYS_OFFSET_EXTENDED`・`PTP_SYS_OFFSET` で読む標本の数
const SAMPLES: u32 = 5;
/// `linux/ptp_clock.h` の `PTP_MAX_SAMPLES`（`ptp_sys_offset_extended` の配列の大きさ）
const PTP_MAX_SAMPLES: usize = 25;
/// `ptp_sys_offset` の配列の大きさ（システム・PHC を交互に並べ、最後はシステム: `2 × PTP_MAX_SAMPLES + 1`）
const BASIC_SLOTS: usize = 2 * PTP_MAX_SAMPLES + 1;
const NANOS_PER_SEC: i128 = 1_000_000_000;
const ZERO_TIME: libc::ptp_clock_time = libc::ptp_clock_time {
    sec: 0,
    nsec: 0,
    reserved: 0,
};

/// PHC とシステムの時計の読み方。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OffsetMethod {
    /// NIC とシステムの時計を同時に読む（対応している NIC だけ）
    Precise,
    /// システムの時計で PHC の読みを挟み、幅が最小の標本を使う
    Extended,
    /// `Extended` と同じだが、挟む時刻がドライバの外（幅が広い）。どの PHC でも使える
    Basic,
}

impl OffsetMethod {
    /// NTP のハードウェアのタイムスタンプで使う読み方（精度の高い順）。
    pub(crate) const FINE: [Self; 2] = [Self::Precise, Self::Extended];
    /// PHC の表示で使う読み方（精度の高い順）。
    pub(crate) const ANY: [Self; 3] = [Self::Precise, Self::Extended, Self::Basic];

    /// 画面に出す名前。
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Precise => "同時読み取り",
            Self::Extended => "前後の読みで挟む",
            Self::Basic => "前後の読みで挟む・粗い",
        }
    }
}

/// PHC を 1 回読んだ結果（どれも Unix ナノ秒。`before`・`after` は REALTIME、`phc` は PHC の時刻）。
/// `PTP_SYS_OFFSET_PRECISE` では `before` と `after` が同じ。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PhcSample {
    pub before: i128,
    pub phc: i128,
    pub after: i128,
}

/// PHC（`/dev/ptpN`）と、システムの時計との組での読み方。
#[derive(Debug)]
pub(crate) struct PhcClock {
    file: File,
    path: String,
    method: OffsetMethod,
}

impl PhcClock {
    /// `/dev/ptp{index}` を開き、`methods` の読み方を順に試して、最初に使えたものを使う。
    pub(crate) fn open(index: u32, methods: &[OffsetMethod]) -> io::Result<Self> {
        let path = format!("/dev/ptp{index}");
        let file = File::open(&path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("{path} を開けません（root が必要です）: {error}"),
            )
        })?;
        let mut clock = Self {
            file,
            path,
            method: OffsetMethod::Precise,
        };
        // 使える読み方を、精度の高いものから順に試す
        let mut last_error = None;
        for &method in methods {
            clock.method = method;
            match clock.read() {
                Ok(_sample) => return Ok(clock),
                Err(error) => last_error = Some(error),
            }
        }
        let error = last_error.unwrap_or_else(|| io::Error::other("読み方がありません"));
        Err(io::Error::new(
            error.kind(),
            format!("{} とシステムの時計を組で読めません: {error}", clock.path),
        ))
    }

    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) const fn method(&self) -> OffsetMethod {
        self.method
    }

    /// PHC とシステムの時計（REALTIME）を組で読む。
    pub(crate) fn read(&self) -> io::Result<PhcSample> {
        match self.method {
            OffsetMethod::Precise => {
                let mut request = libc::ptp_sys_offset_precise {
                    device: ZERO_TIME,
                    sys_realtime: ZERO_TIME,
                    sys_monoraw: ZERO_TIME,
                    rsv: [0; 4],
                };
                ptp_ioctl(&self.file, PtpRequest::Precise(&mut request))?;
                let realtime = nanos(request.sys_realtime);
                Ok(PhcSample {
                    before: realtime,
                    phc: nanos(request.device),
                    after: realtime,
                })
            }
            OffsetMethod::Extended => {
                let mut request = libc::ptp_sys_offset_extended {
                    n_samples: SAMPLES,
                    clockid: libc::CLOCK_REALTIME,
                    rsv: [0; 2],
                    ts: [[ZERO_TIME; 3]; PTP_MAX_SAMPLES],
                };
                ptp_ioctl(&self.file, PtpRequest::Extended(&mut request))?;
                let samples: Vec<[i128; 3]> = request
                    .ts
                    .iter()
                    .take(usize::try_from(request.n_samples).unwrap_or(0))
                    .map(|sample| sample.map(nanos))
                    .collect();
                narrowest_sample(&samples)
            }
            OffsetMethod::Basic => {
                let mut request = libc::ptp_sys_offset {
                    n_samples: SAMPLES,
                    rsv: [0; 3],
                    ts: [ZERO_TIME; BASIC_SLOTS],
                };
                ptp_ioctl(&self.file, PtpRequest::Basic(&mut request))?;
                let count = usize::try_from(request.n_samples).unwrap_or(0);
                let times: Vec<i128> = request
                    .ts
                    .iter()
                    .take(2 * count + 1)
                    .copied()
                    .map(nanos)
                    .collect();
                narrowest_sample(&interleaved(&times))
            }
        }
    }

    /// いまの PHC − REALTIME（ナノ秒）。
    pub(crate) fn phc_minus_realtime(&self) -> io::Result<i128> {
        let sample = self.read()?;
        Ok(sample.phc - sample.before.midpoint(sample.after))
    }
}

/// ptp4l が合わせる PHC を読む口（[`PhcPort`] の実装）。
pub(crate) struct PhcDevice {
    phc: PhcClock,
    clock: Arc<dyn ClockPort>,
}

impl PhcDevice {
    pub(crate) fn open(index: u32, clock: Arc<dyn ClockPort>) -> io::Result<Self> {
        Ok(Self {
            phc: PhcClock::open(index, &OffsetMethod::ANY)?,
            clock,
        })
    }

    pub(crate) fn path(&self) -> &str {
        self.phc.path()
    }

    pub(crate) const fn method(&self) -> OffsetMethod {
        self.phc.method()
    }
}

impl PhcPort for PhcDevice {
    fn read(&mut self) -> Result<PhcReading, PhcReadError> {
        let sample = self
            .phc
            .read()
            .map_err(|error| PhcReadError(format!("{} を読めません: {error}", self.phc.path())))?;
        let observed = self.clock.now();
        match (
            rebase_kernel_time(sample.before, observed),
            rebase_kernel_time(sample.after, observed),
        ) {
            (Some(before), Some(after)) => Ok(PhcReading {
                phc: PhcTime::from_nanos(sample.phc),
                before,
                after,
            }),
            _ => Err(PhcReadError(
                "読んでいる間に PC の時計（CLOCK_REALTIME）が動いたため、使えません".to_owned(),
            )),
        }
    }
}

/// 標本から前後の幅が最小のものを選ぶ。1 つもなければエラー。
fn narrowest_sample(samples: &[[i128; 3]]) -> io::Result<PhcSample> {
    narrowest(samples)
        .map(|[before, phc, after]| PhcSample { before, phc, after })
        .ok_or_else(|| io::Error::other("PHC とシステムの時計の差を測れる標本がありません"))
}

/// システム・PHC・システム・PHC …・システム の並びを、`[前, PHC, 後]` の組にする。
fn interleaved(times: &[i128]) -> Vec<[i128; 3]> {
    times
        .windows(3)
        .step_by(2)
        .filter_map(|window| <[i128; 3]>::try_from(window).ok())
        .collect()
}

/// `[前のシステムの時刻, PHC の時刻, 後のシステムの時刻]` の標本から、前後の幅が最小のものを選ぶ。
/// 前後が逆の標本（読む間にシステムの時計が戻された）は使わない。
fn narrowest(samples: &[[i128; 3]]) -> Option<[i128; 3]> {
    samples
        .iter()
        .filter(|&&[before, _, after]| after >= before)
        .min_by_key(|&&[before, _, after]| after - before)
        .copied()
}

fn nanos(time: libc::ptp_clock_time) -> i128 {
    i128::from(time.sec) * NANOS_PER_SEC + i128::from(time.nsec)
}

/// PHC への ioctl の種類と、渡す構造体（種類ごとに型が決まる）。
enum PtpRequest<'data> {
    Precise(&'data mut libc::ptp_sys_offset_precise),
    Extended(&'data mut libc::ptp_sys_offset_extended),
    Basic(&'data mut libc::ptp_sys_offset),
}

#[expect(
    unsafe_code,
    reason = "PHC とシステムの時計の組は、/dev/ptpN への ioctl でしか読めない"
)]
fn ptp_ioctl(file: &File, request: PtpRequest<'_>) -> io::Result<()> {
    let (code, data): (libc::Ioctl, *mut libc::c_void) = match request {
        PtpRequest::Precise(offset) => (libc::PTP_SYS_OFFSET_PRECISE, ptr::from_mut(offset).cast()),
        PtpRequest::Extended(offset) => {
            (libc::PTP_SYS_OFFSET_EXTENDED, ptr::from_mut(offset).cast())
        }
        PtpRequest::Basic(offset) => (libc::PTP_SYS_OFFSET, ptr::from_mut(offset).cast()),
    };
    // SAFETY: `file` は開いている /dev/ptpN。`data` は要求の種類ごとにカーネルが期待する構造体
    // （ptp_sys_offset_precise / ptp_sys_offset_extended / ptp_sys_offset）を指し、借用でこの呼び出しの間は
    // 生きている。カーネルが書くのはその構造体の範囲（n_samples は PTP_MAX_SAMPLES 以下にしてある）だけ。
    let result = unsafe { libc::ioctl(file.as_raw_fd(), code, data) };
    if result.is_negative() {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_narrowest_extended_sample() {
        let samples = [
            [1_000, 5_000_600, 1_200], // 幅 200
            [2_000, 5_001_550, 2_100], // 幅 100
            [3_000, 5_002_000, 2_900], // 前後が逆（時計が戻された）: 使わない
        ];
        assert_eq!(narrowest(&samples), Some([2_000, 5_001_550, 2_100]));
        assert_eq!(narrowest(&[]), None);
    }

    /// `PTP_SYS_OFFSET` の並び（システム・PHC を交互、最後はシステム）を組にする。
    #[test]
    fn pairs_the_basic_samples() {
        let times = [100, 5_000_150, 300, 5_000_320, 340];
        assert_eq!(
            interleaved(&times),
            [[100, 5_000_150, 300], [300, 5_000_320, 340]]
        );
        let best = narrowest_sample(&interleaved(&times)).unwrap();
        assert_eq!((best.before, best.after), (300, 340));
        assert_eq!(
            narrowest_sample(&interleaved(&[100])).unwrap_err().kind(),
            io::ErrorKind::Other
        );
    }

    #[test]
    fn converts_ptp_clock_time_to_nanoseconds() {
        let time = libc::ptp_clock_time {
            sec: 2,
            nsec: 5,
            reserved: 0,
        };
        assert_eq!(nanos(time), 2_000_000_005);
    }

    #[test]
    fn a_missing_device_is_an_error() {
        let Err(error) = PhcClock::open(u32::MAX, &OffsetMethod::ANY) else {
            panic!("/dev/ptp4294967295 はないはず");
        };
        assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}");
        assert!(error.to_string().contains("/dev/ptp4294967295"), "{error}");
    }
}
