use crate::shared_kernel::{LocalStamp, NtpTimestamp, TimeDelta, TimebaseMismatch};

/// PC の時計に対するサーバ時計のずれ（サーバ − PC）。正ならサーバが進んでいる。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Offset(TimeDelta);

impl Offset {
    pub const fn delta(self) -> TimeDelta {
        self.0
    }
}

/// 往復遅延 = (T4 − T1) − (T3 − T2)。サーバ内の処理時間を除いたネットワークの往復時間。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RoundTripDelay(TimeDelta);

impl RoundTripDelay {
    pub const fn delta(self) -> TimeDelta {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SampleError {
    #[error(transparent)]
    TimebaseMismatch(#[from] TimebaseMismatch),
    #[error("サーバの送信時刻が受信時刻より前です（T3 < T2）")]
    ServerTimeReversed,
    #[error("PC 側の受信時刻が送信時刻より前です（T4 < T1）")]
    LocalTimeReversed,
}

/// 問い合わせ 1 回分の測定結果（T1〜T4）と、そこから求めた offset と delay。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    sent: LocalStamp,
    server_receive: NtpTimestamp,
    server_transmit: NtpTimestamp,
    received: LocalStamp,
    offset: Offset,
    delay: RoundTripDelay,
}

impl Sample {
    /// T1（`sent`）〜 T4（`received`）から標本を作る。
    ///
    /// - delay はモノトニック時計（`mono`）から求める。T1 と T4 は同じ時刻基準でなければならない
    /// - offset は壁時計（`wall`）から求める
    pub fn new(
        sent: LocalStamp,
        server_receive: NtpTimestamp,
        server_transmit: NtpTimestamp,
        received: LocalStamp,
    ) -> Result<Self, SampleError> {
        let local_round_trip = received.mono.since(sent.mono)?;
        if local_round_trip.is_negative() {
            return Err(SampleError::LocalTimeReversed);
        }
        let server_processing = server_transmit.wrapping_since(server_receive);
        if server_processing.is_negative() {
            return Err(SampleError::ServerTimeReversed);
        }
        // PC とサーバの時計の進み方の差でごくわずかに負になりうるので、0 で下限を切る
        let delay = (local_round_trip - server_processing).max(TimeDelta::ZERO);

        // offset = ((T2 − T1) + (T3 − T4)) / 2。和は i128 で計算する
        let forward = server_receive.wrapping_since(sent.wall);
        let backward = server_transmit.wrapping_since(received.wall);
        let offset = TimeDelta::from_i128_nanos(
            (i128::from(forward.as_nanos()) + i128::from(backward.as_nanos())) / 2,
        );

        Ok(Self {
            sent,
            server_receive,
            server_transmit,
            received,
            offset: Offset(offset),
            delay: RoundTripDelay(delay),
        })
    }

    /// T1: 送信時の PC の時刻
    pub const fn sent(&self) -> LocalStamp {
        self.sent
    }

    /// T2: サーバが要求を受け取った時刻
    pub const fn server_receive(&self) -> NtpTimestamp {
        self.server_receive
    }

    /// T3: サーバが応答を送った時刻
    pub const fn server_transmit(&self) -> NtpTimestamp {
        self.server_transmit
    }

    /// T4: 受信時の PC の時刻
    pub const fn received(&self) -> LocalStamp {
        self.received
    }

    pub const fn offset(&self) -> Offset {
        self.offset
    }

    pub const fn delay(&self) -> RoundTripDelay {
        self.delay
    }

    /// T4 の瞬間のサーバ時刻の推定値（T3 + 片道分の遅延）。
    pub fn server_time_at_receive(&self) -> NtpTimestamp {
        self.server_transmit.wrapping_add(self.delay.0 / 2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_kernel::{PcTime, Timebase};

    const WALL_BASE: NtpTimestamp = NtpTimestamp::from_parts(3_900_000_000, 0);

    fn stamp(micros: i64) -> LocalStamp {
        let delta = TimeDelta::from_micros(micros);
        LocalStamp {
            mono: PcTime::monotonic(delta.as_nanos() as u64),
            wall: WALL_BASE.wrapping_add(delta),
        }
    }

    fn at(micros: i64) -> NtpTimestamp {
        WALL_BASE.wrapping_add(TimeDelta::from_micros(micros))
    }

    fn assert_close(actual: TimeDelta, expected: TimeDelta) {
        let diff = (actual - expected).abs();
        assert!(
            diff <= TimeDelta::from_nanos(10),
            "actual {actual:?}, expected {expected:?}"
        );
    }

    #[test]
    fn computes_offset_and_delay() {
        // サーバが 5ms 進んでいて、片道 1ms、サーバ内処理 0.1ms
        let sample = Sample::new(stamp(0), at(6_000), at(6_100), stamp(2_100)).unwrap();
        assert_close(sample.offset().delta(), TimeDelta::from_millis(5));
        assert_close(sample.delay().delta(), TimeDelta::from_millis(2));
        // T4 の瞬間のサーバ時刻 = T3 + 1ms = PC(T4) + 5ms
        assert_close(
            sample
                .server_time_at_receive()
                .wrapping_since(stamp(2_100).wall),
            TimeDelta::from_millis(5),
        );
    }

    #[test]
    fn offset_is_negative_when_pc_is_ahead() {
        let sample = Sample::new(stamp(0), at(-978 + 200), at(-978 + 200), stamp(400)).unwrap();
        assert_close(sample.offset().delta(), TimeDelta::from_micros(-978));
        assert_close(sample.delay().delta(), TimeDelta::from_micros(400));
    }

    #[test]
    fn rejects_mixed_timebases() {
        let received = LocalStamp {
            mono: PcTime::new(Timebase::KernelRealtime, 1_000),
            wall: at(1),
        };
        assert!(matches!(
            Sample::new(stamp(0), at(0), at(0), received),
            Err(SampleError::TimebaseMismatch(_))
        ));
    }

    #[test]
    fn rejects_server_time_going_backwards() {
        assert_eq!(
            Sample::new(stamp(0), at(500), at(400), stamp(1_000)),
            Err(SampleError::ServerTimeReversed)
        );
    }
}
