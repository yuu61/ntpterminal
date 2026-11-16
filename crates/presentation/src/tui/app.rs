//! TUI のイベントループ。

use core::time::Duration;
use std::io;
use std::sync::mpsc::Receiver;
use std::time::Instant;

use ratatui::crossterm::event::{self, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{CompletedFrame, DefaultTerminal};

use application::port::ClockPort;
use application::snapshot::{ReceiverSnapshot, SyncSnapshot};
use domain::time_sync::DisplayClock;

use super::trend::SampleHistory;
use super::{TuiConfig, screen};

/// 1 フレームの長さ（約 60fps）。
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// TUI を動かす。q / Esc / Ctrl+C で終わる。
///
/// `clock` には同期スレッド（と受信機の監視スレッド）と**同じインスタンス**を渡すこと。モノトニック時計の起点が
/// 違うと、時刻基準のチェックでは検出できないまま基準点からの計算が狂う。
/// `receiver` は受信機の監視のスナップショット（`config.receiver` のときだけ渡す）。
///
/// # Errors
///
/// 端末の準備・描画・キー入力の読み取り・後片付けのどれかに失敗したとき。
pub fn run(
    config: &TuiConfig,
    clock: &dyn ClockPort,
    snapshots: &Receiver<SyncSnapshot>,
    receiver: Option<&Receiver<ReceiverSnapshot>>,
) -> io::Result<()> {
    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(error) => {
            drop(ratatui::try_restore());
            return Err(error);
        }
    };
    let result = event_loop(&mut terminal, config, clock, snapshots, receiver);
    let restored = ratatui::try_restore();
    result.and(restored)
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    config: &TuiConfig,
    clock: &dyn ClockPort,
    snapshots: &Receiver<SyncSnapshot>,
    receiver: Option<&Receiver<ReceiverSnapshot>>,
) -> io::Result<()> {
    let mut latest: Option<SyncSnapshot> = None;
    let mut latest_receiver: Option<ReceiverSnapshot> = None;
    let mut history = SampleHistory::default();
    let mut display = DisplayClock::new();
    let mut next_frame = Instant::now();
    loop {
        // 画面には最新のものだけを使う。推移の履歴には、1 フレームの間に 2 回成功しても
        // 落とさないよう、届いたものをすべて通す
        while let Ok(snapshot) = snapshots.try_recv() {
            history.observe(&snapshot);
            latest = Some(snapshot);
        }
        if let Some(channel) = receiver {
            while let Ok(snapshot) = channel.try_recv() {
                latest_receiver = Some(snapshot);
            }
        }
        let now = clock.now().mono;
        let shown = display.tick(
            latest
                .as_ref()
                .and_then(|snapshot| snapshot.anchor.as_ref()),
            now,
        );
        let model = screen::build(
            config,
            latest.as_ref(),
            latest_receiver.as_ref(),
            &history,
            shown,
            now,
        );
        let _: CompletedFrame<'_> =
            terminal.draw(|frame| frame.render_widget(&model, frame.area()))?;

        // 次のフレームの時刻まで入力を待つ（遅れたときは追いつこうとしない）
        next_frame += FRAME_INTERVAL;
        let current = Instant::now();
        if next_frame < current {
            next_frame = current;
        }
        if event::poll(next_frame - current)?
            && event::read()?.as_key_press_event().is_some_and(is_quit)
        {
            return Ok(());
        }
    }
}

/// raw モードでは Ctrl+C がシグナルにならないので、キーとして扱う。
fn is_quit(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}
