use alloc::sync::Arc;
use core::time::Duration;

use domain::packet_crafting::RequestTemplate;
use domain::shared_kernel::TimeDelta;
use domain::time_sync::{RejectReason, Sample, ServerInfo, SourceConfig, SyncEvent};

use super::SyncService;
use crate::port::{ClockPort, NonceSource, PacketPort};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OnceOutcome {
    pub sample: Sample,
    pub server_info: ServerInfo,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OnceError {
    #[error("応答がありませんでした（タイムアウト）")]
    Timeout,
    #[error("応答を採用できませんでした: {0}")]
    Rejected(RejectReason),
    #[error("{0}")]
    Transport(String),
}

/// 1 回だけ問い合わせる。検証は常時同期と同じ集約（`TimeSource`）で行う。
///
/// # Errors
///
/// 時間内に応答がなければ [`OnceError::Timeout`]、応答を採用できなければ [`OnceError::Rejected`]、送受信に失敗したら [`OnceError::Transport`]。
pub fn query_once<P, N>(
    port: P,
    nonce: N,
    clock: Arc<dyn ClockPort>,
    template: RequestTemplate,
    timeout: Duration,
) -> Result<OnceOutcome, OnceError>
where
    P: PacketPort,
    N: NonceSource,
{
    let time_limit = TimeDelta::from_std(timeout).max(TimeDelta::from_millis(1));
    let config = SourceConfig {
        response_timeout: time_limit,
        ..SourceConfig::with_poll_interval(time_limit)
    };
    let mut service = SyncService::new(config, port, nonce, clock, template);
    loop {
        let allow_poll = service.source().stats().attempts == 0;
        for event in service.step(allow_poll, Duration::from_millis(100)) {
            match event {
                SyncEvent::SampleAccepted {
                    sample,
                    server_info,
                    ..
                } => {
                    return Ok(OnceOutcome {
                        sample,
                        server_info,
                    });
                }
                SyncEvent::SampleRejected {
                    seq: Some(_),
                    reason,
                } => return Err(OnceError::Rejected(reason)),
                SyncEvent::ExchangeTimedOut { .. } => return Err(OnceError::Timeout),
                SyncEvent::ExchangeFailed {
                    seq: Some(_),
                    detail,
                } => return Err(OnceError::Transport(detail)),
                SyncEvent::ExchangeStarted { .. }
                | SyncEvent::SampleRejected { .. }
                | SyncEvent::ExchangeFailed { .. }
                | SyncEvent::PollIntervalChanged { .. }
                | SyncEvent::StateChanged { .. } => {}
            }
        }
    }
}
