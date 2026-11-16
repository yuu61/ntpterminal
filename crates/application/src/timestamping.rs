//! タイムスタンプの取得状況（読み取り用）。
//!
//! 送受信（infrastructure）が書き、表示（TUI・stats・once）が読む。スレッドをまたいで共有するので atomic で持つ。
//! 「カーネル（またはハードウェア）の時刻を使う設定にしたのに、実際は取れていない」状態を表示で隠さないためのもの。

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

/// 1 回の測定で使った時刻の出どころ。T1 と T4 で違うときは低い方（ユーザ空間 < カーネル < ハードウェア）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum TimestampSource {
    /// ユーザ空間で時計を読んだ
    #[default]
    User,
    /// カーネルのソフトウェアのタイムスタンプ
    Kernel,
    /// NIC のハードウェアのタイムスタンプ
    Hardware,
}

impl TimestampSource {
    const fn to_u8(self) -> u8 {
        match self {
            Self::User => 0,
            Self::Kernel => 1,
            Self::Hardware => 2,
        }
    }

    const fn from_u8(value: u8) -> Self {
        match value {
            2 => Self::Hardware,
            1 => Self::Kernel,
            _ => Self::User,
        }
    }
}

/// `Arc` で共有する。
#[derive(Debug, Default)]
pub struct TimestampingStatus {
    kernel_enabled: AtomicBool,
    hardware_enabled: AtomicBool,
    hardware_samples: AtomicU64,
    kernel_samples: AtomicU64,
    user_samples: AtomicU64,
    tx_misses: AtomicU64,
    last_source: AtomicU8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimestampingCounts {
    pub kernel_enabled: bool,
    /// NIC のハードウェアのタイムスタンプも使う設定か（`kernel_enabled` も立つ）
    pub hardware_enabled: bool,
    /// T1 と T4 の両方をハードウェアの時刻で取れた受信の数
    pub hardware_samples: u64,
    /// T1 と T4 の両方をカーネルの時刻（少なくとも一方はソフトウェア）で取れた受信の数
    pub kernel_samples: u64,
    /// ユーザ空間の時刻で取った受信の数（カーネルの時刻が取れなかったとき）
    pub user_samples: u64,
    /// 送信時刻（T1）をカーネルから取れなかった送信の数
    pub tx_misses: u64,
    pub last_source: TimestampSource,
}

impl TimestampingCounts {
    /// 設定どおりなら得られるはずの出どころ（ハードウェア > カーネル > ユーザ空間）。
    #[must_use]
    pub const fn expected_source(&self) -> TimestampSource {
        if self.hardware_enabled {
            TimestampSource::Hardware
        } else if self.kernel_enabled {
            TimestampSource::Kernel
        } else {
            TimestampSource::User
        }
    }

    /// 受け入れた受信の数（出どころを問わない）。
    #[must_use]
    pub const fn samples(&self) -> u64 {
        self.hardware_samples + self.kernel_samples + self.user_samples
    }
}

impl TimestampingStatus {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_kernel_enabled(&self, enabled: bool) {
        self.kernel_enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn set_hardware_enabled(&self, enabled: bool) {
        self.hardware_enabled.store(enabled, Ordering::Relaxed);
    }

    /// 受け入れた受信 1 件の時刻の出どころを記録する。
    pub fn record_receive(&self, source: TimestampSource) {
        let counter = match source {
            TimestampSource::Hardware => &self.hardware_samples,
            TimestampSource::Kernel => &self.kernel_samples,
            TimestampSource::User => &self.user_samples,
        };
        let _previous = counter.fetch_add(1, Ordering::Relaxed);
        self.last_source.store(source.to_u8(), Ordering::Relaxed);
    }

    pub fn record_tx_miss(&self) {
        let _previous = self.tx_misses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn counts(&self) -> TimestampingCounts {
        TimestampingCounts {
            kernel_enabled: self.kernel_enabled.load(Ordering::Relaxed),
            hardware_enabled: self.hardware_enabled.load(Ordering::Relaxed),
            hardware_samples: self.hardware_samples.load(Ordering::Relaxed),
            kernel_samples: self.kernel_samples.load(Ordering::Relaxed),
            user_samples: self.user_samples.load(Ordering::Relaxed),
            tx_misses: self.tx_misses.load(Ordering::Relaxed),
            last_source: TimestampSource::from_u8(self.last_source.load(Ordering::Relaxed)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_sources_of_received_samples() {
        let status = TimestampingStatus::new();
        status.set_kernel_enabled(true);
        status.record_receive(TimestampSource::Kernel);
        status.record_receive(TimestampSource::Kernel);
        status.record_tx_miss();
        status.record_receive(TimestampSource::User);
        assert_eq!(
            status.counts(),
            TimestampingCounts {
                kernel_enabled: true,
                hardware_enabled: false,
                hardware_samples: 0,
                kernel_samples: 2,
                user_samples: 1,
                tx_misses: 1,
                last_source: TimestampSource::User,
            }
        );
    }

    #[test]
    fn counts_hardware_samples_separately() {
        let status = TimestampingStatus::new();
        status.set_kernel_enabled(true);
        status.set_hardware_enabled(true);
        status.record_receive(TimestampSource::Hardware);
        status.record_receive(TimestampSource::Kernel);
        status.record_receive(TimestampSource::Hardware);
        let counts = status.counts();
        assert_eq!(counts.hardware_samples, 2);
        assert_eq!(counts.kernel_samples, 1);
        assert_eq!(counts.samples(), 3);
        assert_eq!(counts.last_source, TimestampSource::Hardware);
        assert_eq!(counts.expected_source(), TimestampSource::Hardware);
    }

    #[test]
    fn expected_source_follows_the_settings() {
        let mut counts = TimestampingCounts::default();
        assert_eq!(counts.expected_source(), TimestampSource::User);
        counts.kernel_enabled = true;
        assert_eq!(counts.expected_source(), TimestampSource::Kernel);
        counts.hardware_enabled = true;
        assert_eq!(counts.expected_source(), TimestampSource::Hardware);
    }

    #[test]
    fn source_round_trips_through_its_stored_form() {
        for source in [
            TimestampSource::User,
            TimestampSource::Kernel,
            TimestampSource::Hardware,
        ] {
            assert_eq!(TimestampSource::from_u8(source.to_u8()), source);
        }
        assert!(TimestampSource::User < TimestampSource::Kernel);
        assert!(TimestampSource::Kernel < TimestampSource::Hardware);
    }
}
