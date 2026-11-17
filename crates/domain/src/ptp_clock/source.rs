//! 集約ルート [`PhcSource`]（ptp4l が合わせる PHC 1 つ）。
//!
//! - 問い合わせごとに、ptp4l の状態（[`PtpStatus`]）と PHC の読み取り（[`PhcReading`]）を受け取る。
//!   状態が読めなくても PHC は読み続け、最後に読めた時刻の性質（UTC との差）で換算する（ptp4l が止まっても
//!   時計を止めない）
//! - 同期の状態は ptp4l のポートの状態から決める（[`PhcState`]）。PHC だけでは同期しているか分からないので、
//!   ptp4l が SLAVE かマスタと知らせるまでは、基準点を表示に出さない（合わせられていない PHC は任意の時刻で進む）。
//!   PC との差（offset）の標本は、その間も取り続ける
//! - 一度同期した後に同期しなくなった（マスタを見失った・ptp4l の応答がない）ときは、自走中として表示を続ける。
//!   PHC は ptp4l が最後に合わせた速さで進み続けるため
//! - 問い合わせの予定は NTP の時刻源と同じく、前回の予定の時刻から数える（1 回分以上遅れたら送った時刻から）

use super::{ClockRole, PhcReading, PtpStatus};
use crate::shared_kernel::{PcTime, TimeDelta};
use crate::time_sync::{Anchor, ClockTracker, FrequencyOffset, Sample};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhcConfig {
    pub poll_interval: TimeDelta,
    /// クロックフィルタが保持する標本の数
    pub filter_len: usize,
}

impl PhcConfig {
    #[must_use]
    pub fn with_poll_interval(poll_interval: TimeDelta) -> Self {
        Self {
            poll_interval: poll_interval.max(TimeDelta::from_millis(10)),
            filter_len: 8,
        }
    }
}

/// 同期の状態。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PhcState {
    /// ptp4l の状態をまだ読めていない
    Starting,
    /// ptp4l がマスタを探している・合わせている途中で、まだ一度も同期していない
    Acquiring,
    /// ptp4l がマスタに同期している（SLAVE のポートがある）
    Synchronized,
    /// ptp4l がマスタとして時刻を配っている（この PHC がほかの機器の基準）
    Master,
    /// 同期していたが、今は同期していない（マスタを見失った・ptp4l の応答がない）。PHC は進み続ける
    Holdover,
    /// ポートが使えない（FAULTY・DISABLED）まま、まだ一度も同期していない
    Faulty,
}

impl PhcState {
    /// PHC の時刻を表示してよい状態か。
    #[must_use]
    pub const fn shows_time(self) -> bool {
        matches!(self, Self::Synchronized | Self::Master | Self::Holdover)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhcStats {
    /// 問い合わせた回数
    pub attempts: u64,
    /// PHC を読んで標本にできた回数
    pub readings: u64,
    /// PHC を読めなかった・標本にできなかった回数
    pub read_failures: u64,
    /// ptp4l の状態を読めた回数
    pub status_updates: u64,
    /// ptp4l の状態を読めなかった回数
    pub status_failures: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PhcEvent {
    StatusReceived {
        seq: u64,
    },
    StatusFailed {
        seq: u64,
        detail: String,
    },
    ReadingAccepted {
        seq: u64,
        sample: Sample,
    },
    ReadingFailed {
        seq: u64,
        detail: String,
    },
    /// PHC の時刻とモノトニック時計の関係が段差で変わった（ptp4l が PHC を合わせ直した・UTC との差が変わった・
    /// PC のスリープなど）。それまでの標本を捨て、新しい時刻に合わせた
    ClockStepDetected {
        step: TimeDelta,
    },
    StateChanged {
        from: PhcState,
        to: PhcState,
    },
}

/// 時刻はすべて引数で受け取り（モノトニック時計）、起きたことを [`PhcEvent`] で返す。
#[derive(Clone, Debug)]
pub struct PhcSource {
    config: PhcConfig,
    state: PhcState,
    next_poll: PcTime,
    tracker: ClockTracker,
    last_sample: Option<Sample>,
    status: Option<PtpStatus>,
    /// 直近の状態の問い合わせが失敗した理由。読めたら `None` に戻る
    status_error: Option<String>,
    last_success: Option<PcTime>,
    stats: PhcStats,
    next_seq: u64,
    /// 一度でも同期した（SLAVE かマスタになった）か
    disciplined_once: bool,
}

impl PhcSource {
    /// `now` の時点ですぐに最初の問い合わせを行う状態で作る。
    #[must_use]
    pub fn new(config: PhcConfig, now: PcTime) -> Self {
        Self {
            config,
            state: PhcState::Starting,
            next_poll: now,
            tracker: ClockTracker::new(config.filter_len),
            last_sample: None,
            status: None,
            status_error: None,
            last_success: None,
            stats: PhcStats::default(),
            next_seq: 1,
            disciplined_once: false,
        }
    }

    // ---- 参照 ----

    #[must_use]
    pub const fn state(&self) -> PhcState {
        self.state
    }

    #[must_use]
    pub const fn stats(&self) -> PhcStats {
        self.stats
    }

    #[must_use]
    pub const fn poll_interval(&self) -> TimeDelta {
        self.config.poll_interval
    }

    /// 大きい時計の表示に使う基準点。表示してよい状態（[`PhcState::shows_time`]）のときだけ返す。
    #[must_use]
    pub const fn anchor(&self) -> Option<Anchor> {
        if self.state.shows_time() {
            self.tracker.anchor()
        } else {
            None
        }
    }

    #[must_use]
    pub const fn last_sample(&self) -> Option<Sample> {
        self.last_sample
    }

    /// クロックフィルタが選んだ標本。
    #[must_use]
    pub fn selected_sample(&self) -> Option<Sample> {
        self.tracker.selected()
    }

    /// PHC がモノトニック時計より速く進む割合の推定値。まだ推定できていなければ `None`。
    #[must_use]
    pub const fn frequency(&self) -> Option<FrequencyOffset> {
        self.tracker.frequency()
    }

    /// 最後に読めた ptp4l の状態（直近の問い合わせが失敗していても残る。[`Self::status_error`] を見ること）。
    #[must_use]
    pub const fn status(&self) -> Option<&PtpStatus> {
        self.status.as_ref()
    }

    #[must_use]
    pub fn status_error(&self) -> Option<&str> {
        self.status_error.as_deref()
    }

    /// 最後に PHC を読めた時刻。
    #[must_use]
    pub const fn last_success(&self) -> Option<PcTime> {
        self.last_success
    }

    #[must_use]
    pub const fn next_poll(&self) -> PcTime {
        self.next_poll
    }

    #[must_use]
    pub fn is_poll_due(&self, now: PcTime) -> bool {
        now.is_at_or_after(self.next_poll).unwrap_or(false)
    }

    /// PHC の時刻を表示用に換算できるか（ptp4l の状態を一度でも読めたか）。
    #[must_use]
    pub const fn can_convert(&self) -> bool {
        self.status.is_some()
    }

    // ---- コマンド ----

    /// `now` に問い合わせを始める。番号を返す。
    pub fn begin_poll(&mut self, now: PcTime) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.stats.attempts += 1;
        let on_schedule = self.next_poll.saturating_add(self.config.poll_interval);
        self.next_poll = if now.is_at_or_after(on_schedule) == Ok(false) {
            on_schedule
        } else {
            now.saturating_add(self.config.poll_interval)
        };
        seq
    }

    /// ptp4l の状態を読めた。
    pub fn status_received(&mut self, seq: u64, status: PtpStatus) -> Vec<PhcEvent> {
        let mut events = vec![PhcEvent::StatusReceived { seq }];
        self.stats.status_updates += 1;
        let to = match status.role() {
            ClockRole::Slave => PhcState::Synchronized,
            ClockRole::Master => PhcState::Master,
            ClockRole::Uncalibrated | ClockRole::Listening | ClockRole::Faulty
                if self.disciplined_once =>
            {
                PhcState::Holdover
            }
            ClockRole::Uncalibrated | ClockRole::Listening => PhcState::Acquiring,
            ClockRole::Faulty => PhcState::Faulty,
        };
        if matches!(to, PhcState::Synchronized | PhcState::Master) {
            self.disciplined_once = true;
        }
        self.status = Some(status);
        self.status_error = None;
        self.transition(to, &mut events);
        events
    }

    /// ptp4l の状態を読めなかった。同期していたなら自走中にする（最後の状態は残す）。
    pub fn status_failed(&mut self, seq: u64, detail: String) -> Vec<PhcEvent> {
        self.stats.status_failures += 1;
        self.status_error = Some(detail.clone());
        let mut events = vec![PhcEvent::StatusFailed { seq, detail }];
        if matches!(self.state, PhcState::Synchronized | PhcState::Master) {
            self.transition(PhcState::Holdover, &mut events);
        }
        events
    }

    /// PHC を読めた。最後に読めた ptp4l の時刻の性質で換算し、標本にする。
    pub fn reading_received(&mut self, seq: u64, reading: PhcReading) -> Vec<PhcEvent> {
        let Some(status) = self.status.as_ref() else {
            return self.reading_failed(
                seq,
                "ptp4l の状態（UTC との差）をまだ読めていないので、PHC の時刻を換算できません"
                    .to_owned(),
            );
        };
        let shown = reading.phc.to_display_time(&status.time);
        match Sample::new(reading.before, shown, shown, reading.after) {
            Ok(sample) => {
                self.stats.readings += 1;
                self.last_sample = Some(sample);
                self.last_success = Some(sample.received().mono);
                let mut events = vec![PhcEvent::ReadingAccepted { seq, sample }];
                if let Some(step) = self.tracker.track(sample) {
                    events.push(PhcEvent::ClockStepDetected { step });
                }
                events
            }
            Err(error) => self.reading_failed(seq, error.to_string()),
        }
    }

    /// PHC を読めなかった。
    pub fn reading_failed(&mut self, seq: u64, detail: String) -> Vec<PhcEvent> {
        self.stats.read_failures += 1;
        vec![PhcEvent::ReadingFailed { seq, detail }]
    }

    fn transition(&mut self, to: PhcState, events: &mut Vec<PhcEvent>) {
        if self.state != to {
            events.push(PhcEvent::StateChanged {
                from: self.state,
                to,
            });
            self.state = to;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptp_clock::{
        ClockIdentity, ClockQuality, LeapWarning, PhcTime, PortState, PortStatus, TimeProperties,
        TimeSourceKind, Timescale,
    };
    use crate::shared_kernel::{LocalStamp, NtpTimestamp};

    const NANOS_PER_SEC: i128 = 1_000_000_000;
    /// PC の壁時計の起点（2026-11-17 00:00:00 UTC の Unix 秒）
    const WALL_BASE_SECS: i128 = 1_794_873_600;

    fn stamp(micros: i64) -> LocalStamp {
        let delta = TimeDelta::from_micros(micros);
        LocalStamp {
            mono: PcTime::monotonic(delta.as_nanos().cast_unsigned()),
            wall: NtpTimestamp::from_unix_nanos(WALL_BASE_SECS * NANOS_PER_SEC).wrapping_add(delta),
        }
    }

    fn status(state: PortState, utc_offset: i16) -> PtpStatus {
        PtpStatus {
            domain: 0,
            ports: vec![PortStatus { number: 1, state }],
            offset_from_master: TimeDelta::from_nanos(-12),
            mean_path_delay: TimeDelta::from_nanos(1_234),
            steps_removed: 1,
            grandmaster: ClockIdentity([0, 0x1B, 0x21, 0xFF, 0xFE, 0x12, 0x34, 0x56]),
            grandmaster_quality: ClockQuality {
                class: 6,
                accuracy: 0x21,
                variance: 0x4E5D,
            },
            time: TimeProperties {
                utc_offset,
                utc_offset_valid: true,
                timescale: Timescale::Ptp,
                leap: LeapWarning::None,
                time_traceable: true,
                frequency_traceable: true,
                time_source: TimeSourceKind::Gnss,
            },
        }
    }

    /// PC の壁時計より `ahead_us` 進んだ UTC を、TAI（+37 秒）で持つ PHC を、幅 `width_us` で読む。
    fn reading(at_us: i64, ahead_us: i64, width_us: i64) -> PhcReading {
        let before = stamp(at_us);
        let after = stamp(at_us + width_us);
        let middle = before
            .wall
            .wrapping_add(TimeDelta::from_micros(width_us / 2 + ahead_us));
        PhcReading {
            phc: PhcTime::from_nanos(middle.to_unix_nanos() + 37 * NANOS_PER_SEC),
            before,
            after,
        }
    }

    fn source() -> PhcSource {
        PhcSource::new(
            PhcConfig::with_poll_interval(TimeDelta::from_millis(500)),
            stamp(0).mono,
        )
    }

    fn poll(src: &mut PhcSource, at_us: i64, state: PortState, ahead_us: i64) -> Vec<PhcEvent> {
        let seq = src.begin_poll(stamp(at_us).mono);
        let mut events = src.status_received(seq, status(state, 37));
        events.extend(src.reading_received(seq, reading(at_us, ahead_us, 2)));
        events
    }

    #[test]
    fn shows_utc_once_ptp4l_is_slave() {
        let mut src = source();
        assert!(src.is_poll_due(stamp(0).mono));
        let events = poll(&mut src, 0, PortState::Slave, 300);
        assert!(events.contains(&PhcEvent::StateChanged {
            from: PhcState::Starting,
            to: PhcState::Synchronized,
        }));
        let sample = src.last_sample().unwrap();
        // TAI の 37 秒を引いた UTC で、PC より 0.3 ms 進んでいる。読み取りの幅が delay になる
        let offset_error = sample.offset().delta() - TimeDelta::from_micros(300);
        assert!(
            offset_error.abs() <= TimeDelta::from_nanos(10),
            "{offset_error:?}"
        );
        assert_eq!(sample.delay().delta(), TimeDelta::from_micros(2));
        let later = stamp(1_000_000);
        let shown = src.anchor().unwrap().project(later.mono).unwrap();
        let error = shown.wrapping_since(later.wall) - TimeDelta::from_micros(300);
        assert!(error.abs() <= TimeDelta::from_nanos(10), "{error:?}");
        assert_eq!(src.stats().readings, 1);
        assert_eq!(src.stats().status_updates, 1);
        assert!(!src.is_poll_due(stamp(499_999).mono));
        assert!(src.is_poll_due(stamp(500_000).mono));
    }

    #[test]
    fn hides_the_clock_until_ptp4l_has_synchronized() {
        let mut src = source();
        let events = poll(&mut src, 0, PortState::Listening, 650_000);
        assert!(events.contains(&PhcEvent::StateChanged {
            from: PhcState::Starting,
            to: PhcState::Acquiring,
        }));
        // PC との差の標本は取るが、時計には出さない
        assert!(src.last_sample().is_some());
        assert_eq!(src.anchor(), None);
        drop(poll(&mut src, 500_000, PortState::Uncalibrated, 650_000));
        assert_eq!(src.state(), PhcState::Acquiring);
        assert_eq!(src.anchor(), None);

        let mut faulty = source();
        drop(poll(&mut faulty, 0, PortState::Faulty, 0));
        assert_eq!(faulty.state(), PhcState::Faulty);
        assert_eq!(faulty.anchor(), None);
    }

    #[test]
    fn keeps_the_clock_in_holdover_after_losing_the_master() {
        let mut src = source();
        drop(poll(&mut src, 0, PortState::Slave, 0));
        let events = poll(&mut src, 500_000, PortState::Listening, 0);
        assert!(events.contains(&PhcEvent::StateChanged {
            from: PhcState::Synchronized,
            to: PhcState::Holdover,
        }));
        assert!(src.anchor().is_some());
        drop(poll(&mut src, 1_000_000, PortState::Slave, 0));
        assert_eq!(src.state(), PhcState::Synchronized);
    }

    #[test]
    fn keeps_reading_the_phc_while_ptp4l_does_not_answer() {
        let mut src = source();
        drop(poll(&mut src, 0, PortState::Slave, 0));
        let seq = src.begin_poll(stamp(500_000).mono);
        let events = src.status_failed(seq, "応答なし".to_owned());
        assert_eq!(
            events,
            vec![
                PhcEvent::StatusFailed {
                    seq,
                    detail: "応答なし".to_owned(),
                },
                PhcEvent::StateChanged {
                    from: PhcState::Synchronized,
                    to: PhcState::Holdover,
                },
            ]
        );
        assert_eq!(src.status_error(), Some("応答なし"));
        // 最後に読めた UTC との差で換算し続ける
        let accepted = src.reading_received(seq, reading(500_000, 0, 2));
        assert!(matches!(accepted[0], PhcEvent::ReadingAccepted { .. }));
        assert_eq!(src.stats().readings, 2);
        assert_eq!(src.stats().status_failures, 1);
        assert!(src.anchor().is_some());

        // 読めるようになったら戻る
        let next = src.begin_poll(stamp(1_000_000).mono);
        drop(src.status_received(next, status(PortState::Slave, 37)));
        assert_eq!(src.status_error(), None);
        assert_eq!(src.state(), PhcState::Synchronized);
    }

    #[test]
    fn cannot_convert_before_the_first_status() {
        let mut src = source();
        assert!(!src.can_convert());
        let seq = src.begin_poll(stamp(0).mono);
        drop(src.status_failed(seq, "応答なし".to_owned()));
        assert_eq!(src.state(), PhcState::Starting);
        let events = src.reading_received(seq, reading(0, 0, 2));
        assert!(matches!(events[0], PhcEvent::ReadingFailed { .. }));
        assert_eq!(src.stats().read_failures, 1);
        assert_eq!(src.last_sample(), None);
    }

    #[test]
    fn a_master_shows_its_own_time() {
        let mut src = source();
        drop(poll(&mut src, 0, PortState::GrandMaster, 0));
        assert_eq!(src.state(), PhcState::Master);
        assert!(src.anchor().is_some());
    }

    /// ptp4l が PHC を 0.65 秒合わせ直した（2 回続けば段差として新しい時刻に合わせる）。
    #[test]
    fn follows_a_step_of_the_phc() {
        let mut src = source();
        for index in 0..4 {
            drop(poll(&mut src, index * 500_000, PortState::Slave, 0));
        }
        assert!(
            !poll(&mut src, 2_000_000, PortState::Slave, -650_000)
                .iter()
                .any(|event| matches!(event, PhcEvent::ClockStepDetected { .. }))
        );
        let events = poll(&mut src, 2_500_000, PortState::Slave, -650_000);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, PhcEvent::ClockStepDetected { .. }))
        );
        let now = stamp(2_600_000);
        let shown = src.anchor().unwrap().project(now.mono).unwrap();
        let error = shown.wrapping_since(now.wall) + TimeDelta::from_millis(650);
        assert!(error.abs() <= TimeDelta::from_micros(1), "{error:?}");
    }
}
