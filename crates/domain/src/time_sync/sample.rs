use crate::shared_kernel::{LocalStamp, NtpTimestamp, TimeDelta, TimebaseMismatch};

/// PC の時計に対するサーバ時計のずれ（サーバ − PC）。正ならサーバが進んでいる。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Offset(TimeDelta);

impl Offset {
    #[must_use]
    pub const fn delta(self) -> TimeDelta {
        self.0
    }
}

/// 往復遅延 = (T4 − T1) − (T3 − T2)。サーバ内の処理時間を除いたネットワークの往復時間。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RoundTripDelay(TimeDelta);

impl RoundTripDelay {
    #[must_use]
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
    ///
    /// # Errors
    ///
    /// T1 と T4 の時刻基準が違う、T3 < T2、または T4 < T1 のとき。
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

        // offset = ((T2 − T1) + (T3 − T4)) / 2。midpoint は和があふれず、/ 2 と同じく 0 方向に丸める
        let forward = server_receive.wrapping_since(sent.wall);
        let backward = server_transmit.wrapping_since(received.wall);
        let offset = TimeDelta::from_nanos(i64::midpoint(forward.as_nanos(), backward.as_nanos()));

        Ok(Self {
            sent,
            server_receive,
            server_transmit,
            received,
            offset: Offset(offset),
            delay: RoundTripDelay(delay),
        })
    }

    /// PTP の 1 回の交換（マスタの Sync と、PC の `Delay_Req`）から標本を作る。
    ///
    /// - `sync_origin`（t1）: マスタが Sync を送った時刻（`correctionField` を足したもの）
    /// - `sync_received`（t2）: PC が Sync を受け取った時刻
    /// - `delay_req_sent`（t3）: PC が `Delay_Req` を送った時刻
    /// - `delay_req_received`（t4）: マスタが `Delay_Req` を受け取った時刻（`Delay_Resp` の `correctionField` を引いたもの）
    ///
    /// NTP と送受信の向きが逆（マスタ → PC が先）なので、[`Self::new`] の順序の確認はしない。
    /// 往復遅延は (t4 − t1) − (t3 − t2)（PC の側はモノトニック時計）、offset は ((t4 − t3) + (t1 − t2)) / 2
    /// （壁時計）で、どちらも NTP と同じ式になる。T1〜T4 は T1 = t3・T2 = t4・T3 = t1・T4 = t2 として持つ
    /// （どれも実測の値）。基準点は、T4（Sync を受け取った瞬間）にマスタ時刻 t1 + 往復遅延 / 2 を対応させる。
    /// t2 と t3 が離れているほど、その間の PC とマスタの時計の進み方の差が往復遅延に入るので、`Delay_Req` は
    /// Sync を受け取った直後に送ること。
    ///
    /// # Errors
    ///
    /// t2 と t3 の時刻基準が違うとき。
    pub fn from_ptp_exchange(
        sync_origin: NtpTimestamp,
        sync_received: LocalStamp,
        delay_req_sent: LocalStamp,
        delay_req_received: NtpTimestamp,
    ) -> Result<Self, SampleError> {
        let local_gap = delay_req_sent.mono.since(sync_received.mono)?;
        let master_span = delay_req_received.wrapping_since(sync_origin);
        // 経路の非対称やタイムスタンプの揺らぎで負になりうるので、0 で下限を切る
        let delay = (master_span - local_gap).max(TimeDelta::ZERO);
        let forward = delay_req_received.wrapping_since(delay_req_sent.wall);
        let backward = sync_origin.wrapping_since(sync_received.wall);
        let offset = TimeDelta::from_nanos(i64::midpoint(forward.as_nanos(), backward.as_nanos()));
        Ok(Self {
            sent: delay_req_sent,
            server_receive: delay_req_received,
            server_transmit: sync_origin,
            received: sync_received,
            offset: Offset(offset),
            delay: RoundTripDelay(delay),
        })
    }

    /// T1: 送信時の PC の時刻
    #[must_use]
    pub const fn sent(&self) -> LocalStamp {
        self.sent
    }

    /// T2: サーバが要求を受け取った時刻
    #[must_use]
    pub const fn server_receive(&self) -> NtpTimestamp {
        self.server_receive
    }

    /// T3: サーバが応答を送った時刻
    #[must_use]
    pub const fn server_transmit(&self) -> NtpTimestamp {
        self.server_transmit
    }

    /// T4: 受信時の PC の時刻
    #[must_use]
    pub const fn received(&self) -> LocalStamp {
        self.received
    }

    #[must_use]
    pub const fn offset(&self) -> Offset {
        self.offset
    }

    #[must_use]
    pub const fn delay(&self) -> RoundTripDelay {
        self.delay
    }

    /// T4 の瞬間のサーバ時刻の推定値（T3 + 片道分の遅延）。
    #[must_use]
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
            mono: PcTime::monotonic(delta.as_nanos().cast_unsigned()),
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

    /// マスタが 5ms 進んでいて、片道 1ms。PC の 1ms に Sync を受け取り、3ms に `Delay_Req` を送る。
    #[test]
    fn computes_a_ptp_exchange() {
        let sample =
            Sample::from_ptp_exchange(at(5_000), stamp(1_000), stamp(3_000), at(9_000)).unwrap();
        assert_close(sample.offset().delta(), TimeDelta::from_millis(5));
        assert_close(sample.delay().delta(), TimeDelta::from_millis(2));
        assert_eq!(sample.received(), stamp(1_000));
        assert_eq!(sample.sent(), stamp(3_000));
        // Sync を受け取った瞬間のマスタ時刻 = t1 + 1ms = PC(t2) + 5ms
        assert_close(
            sample
                .server_time_at_receive()
                .wrapping_since(stamp(1_000).wall),
            TimeDelta::from_millis(5),
        );
        // Delay_Req が Sync より先でも同じ値になる（PC の 0ms に送り、マスタの 6ms に届く）
        let earlier =
            Sample::from_ptp_exchange(at(5_000), stamp(1_000), stamp(0), at(6_000)).unwrap();
        assert_close(earlier.offset().delta(), TimeDelta::from_millis(5));
        assert_close(earlier.delay().delta(), TimeDelta::from_millis(2));
    }

    #[test]
    fn ptp_delay_never_goes_negative() {
        // タイムスタンプの揺らぎで、PC の側の間隔（2ms）がマスタの側（1ms）より長く見える
        let sample =
            Sample::from_ptp_exchange(at(5_000), stamp(1_000), stamp(3_000), at(6_000)).unwrap();
        assert_eq!(sample.delay().delta(), TimeDelta::ZERO);
        let other_timebase = LocalStamp {
            mono: PcTime::new(Timebase::KernelRealtime, 3_000_000),
            wall: at(3_000),
        };
        assert!(matches!(
            Sample::from_ptp_exchange(at(5_000), stamp(1_000), other_timebase, at(9_000)),
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
