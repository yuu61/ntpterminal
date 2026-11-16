//! TUI のイベントループ。

use std::io;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, KeyCode, KeyEvent, KeyModifiers};

use application::port::ClockPort;
use application::snapshot::SyncSnapshot;
use domain::time_sync::DisplayClock;

use super::{TuiConfig, screen};

/// 1 フレームの長さ（約 60fps）。
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// TUI を動かす。q / Esc / Ctrl+C で終わる。
///
/// `clock` には同期スレッドと**同じインスタンス**を渡すこと。モノトニック時計の起点が違うと、
/// 時刻基準のチェックでは検出できないまま基準点からの計算が狂う。
pub fn run(
    config: &TuiConfig,
    clock: Arc<dyn ClockPort>,
    snapshots: Receiver<SyncSnapshot>,
) -> io::Result<()> {
    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = ratatui::try_restore();
            return Err(error);
        }
    };
    let result = event_loop(&mut terminal, config, clock.as_ref(), &snapshots);
    let restored = ratatui::try_restore();
    result.and(restored)
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    config: &TuiConfig,
    clock: &dyn ClockPort,
    snapshots: &Receiver<SyncSnapshot>,
) -> io::Result<()> {
    let mut latest: Option<SyncSnapshot> = None;
    let mut display = DisplayClock::new();
    let mut next_frame = Instant::now();
    loop {
        // 届いているスナップショットのうち最新のものだけを使う
        while let Ok(snapshot) = snapshots.try_recv() {
            latest = Some(snapshot);
        }
        let now = clock.now().mono;
        let shown = display.tick(latest.as_ref().and_then(|s| s.anchor.as_ref()), now);
        let model = screen::build(config, latest.as_ref(), shown, now);
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
