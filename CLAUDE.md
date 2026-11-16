# ntpterminal

GNSS タイムサーバ（スポンサー機）のデモ用 NTP クロック。ローカルのサーバに 1 秒未満の間隔で問い合わせ、
サーバ時刻を大きな数字で表示する TUI を作る。Linux 推奨、Windows でも動く位置づけ。

## コマンド

```sh
cargo build
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all

# 実行例
cargo run -- once 192.168.92.234
cargo run -- once 192.168.92.234 --json
cargo run -- stats 192.168.92.234 --interval-ms 500 --count 20
cargo run -- sim-server --offset-ms -0.978      # 実機がないとき。別の端末で ↓
cargo run -- stats 127.0.0.1:12300
```

## アーキテクチャ（DDD + レイヤーごとのクレート）

| クレート（`crates/`） | 役割 | 依存してよいもの |
|---|---|---|
| `domain` | 純粋なモデル。I/O・OS・時計の読み取りなし | `thiserror` のみ |
| `application` | ユースケース、ポート（trait）、ACL、読み取り用モデル | `domain` |
| `infrastructure` | ポートの実装（UDP・時計・ノンス・疑似サーバ）。**OS 固有のコードはここだけ** | `domain`, `application`, OS 系クレート |
| `presentation` | CLI 出力（M1）、TUI（M2） | `domain`, `application`, 表示系クレート |
| ルート `src/main.rs` | 組み立て役（composition root）。ロジックを書かない | すべて |

- 依存の向きは外 → 内のみ。逆向きは Cargo.toml に書かない（＝コンパイルできない）。
- `domain` / `application` / `presentation` は `#![forbid(unsafe_code)]`。unsafe は `infrastructure` だけ。
- OS 固有のコードは `#[cfg(target_os = "...")]` と `[target.'cfg(...)'.dependencies]` で分ける。
- 依存は **使う時点で** 追加する。バージョンはルート Cargo.toml の `[workspace.dependencies]` で一元管理し、
  各クレートは `xxx.workspace = true` で参照する。

### 境界づけられたコンテキスト（`domain` 内の module）

- `time_sync`（コアドメイン）: 集約ルート `TimeSource`（サーバ 1 台 = 1 インスタンス）。
  I/O を持たず、入力を受けて `Vec<SyncEvent>` を返す。**応答の検証ルールはすべてここ。**
- `packet_crafting`（支援サブドメイン）: `NtpMessage` と encode / decode。不正な値も表現できる。検証しない。
- `shared_kernel`: 両者が共有する時刻型のみ。
- `time_sync` から `packet_crafting` を参照しない。翻訳は `application::acl` が行う（規約。破られるようならクレートを分ける）。

## 用語集（ユビキタス言語）

| 日本語 | コード | 意味 |
|---|---|---|
| 時刻源 | `TimeSource` | 問い合わせ先のサーバ 1 台（集約ルート） |
| 問い合わせ | exchange | 要求を 1 回送り、応答を 1 回受け取るまで。`seq` で番号付け |
| 標本 | `Sample` | 問い合わせ 1 回分の T1〜T4 と、そこから求めた offset / delay |
| オフセット | `Offset` | サーバ − PC。正ならサーバが進んでいる |
| 往復遅延 | `RoundTripDelay` | (T4 − T1) − (T3 − T2) |
| 基準点 | `Anchor` | サーバ時刻とモノトニック時計の組。大きい時計はここから進めて表示する |
| 自走 | `Holdover` | 応答が途絶えた状態。基準点から時刻を進め続ける |
| クロックフィルタ | `ClockFilter` | 直近 N 件から往復遅延が最小の標本を選ぶ |
| 時刻基準 | `Timebase` | PC 側の時刻をどの時計で取ったか（モノトニック / カーネル / QPC / NIC） |
| ノンス | nonce | 要求の Transmit Timestamp に入れる乱数。応答の Originate と照合する |
| 無関係な受信 | stray | 自分の問い合わせと一致しない受信（遅延到着など）。成否に数えない |
| 送信モード | （M3〜）`udp` / `raw-ip` / `link` | パケットをどの層から作るか |

## 時刻の扱い（最重要）

- 時刻は 3 種類。**混ぜない。**
  1. サーバ時刻 … `NtpTimestamp`（T2, T3）
  2. PC のモノトニック時計 … `PcTime`（`Timebase::Monotonic`）… delay・基準点・スケジュールに使う
  3. PC の壁時計 … `LocalStamp.wall` … **offset の計算にだけ**使う
- `PcTime` は必ず `Timebase` を持つ。違う `Timebase` どうしの差は `TimebaseMismatch` エラー（実行時に検証）。
  送信モードは実行時に選ぶので、型パラメータでの強制はしない。
- ドメインは `now()` を呼ばない。時刻は必ず引数で受け取る（`ClockPort` は infrastructure が実装）。
- `Anchor` は必ず Monotonic。カーネル / NIC の時刻は採用前に変換する（M4 で `TimebaseMapper` ポートを追加予定）。
- NTP 時刻の差は `wrapping_since` / `wrapping_add` を使う（2036-02-07 の era 一周をまたいでも正しい）。
  絶対時刻への変換は `to_unix_nanos`（秒 < 0x8000_0000 を era 1 とみなすピボット方式）。
- 固定小数 ↔ ナノ秒の変換は最も近い値に丸める（往復で値が変わらないように）。

## コーディング規約

- module は `foo.rs` + `foo/` 形式（`mod.rs` 禁止。clippy `mod_module_files` で強制）。
- エラー型: ライブラリは `thiserror`、`src/main.rs` だけ `anyhow`。
- コメント・UI 文言は日本語。
- 単体テストは同じファイルの `#[cfg(test)] mod tests`、結合テストは各クレートの `tests/`。
- パケットの組み立て / 解析は、バイト列の正解値と比べるテストを書く。
- application のテストは仮想時間（偽の `ClockPort` + 模擬 `PacketPort`）で書き、実時間を待たない。

## ネットワークの作法

- 1 秒未満のポーリングは自分で管理するローカルのサーバ専用。公開サーバには 16 秒未満で連続して問い合わせない
  （`application::polling_policy` が拒否する。`--allow-fast-poll` で解除）。
- テスト用のサーバは `127.0.0.1` に bind する（Windows のファイアウォール確認を避ける）。
- Windows: UDP の受信タイムアウトは `TimedOut`（Unix は `WouldBlock`）。ICMP 到達不能のあとは
  `ConnectionReset`（Linux は `ConnectionRefused`）。どれも致命的エラーにしない。

## マイルストーン

1. **M1（済）**: NTP コア + UDP + CLI（`once` / `stats` / `sim-server`）
2. M2: TUI（ratatui）… 大きい数字（半ブロック）・情報欄・Sparkline。
   ratatui はバージョンを固定し、crossterm は単独で入れず `ratatui::crossterm` を使う。
   同期は別スレッドで回し、`SyncSnapshot` をチャネルで UI に渡す（集約を共有しない）。
   クロックフィルタが基準点を切り替えても、**大きい時計の表示は逆戻りさせない**（差は少しずつ吸収する）。
3. M3: `raw-ip` モード（Linux）… IP / UDP ヘッダの生成（`packet_crafting` にヘッダ指定を追加）
4. M4: `link` モード、カーネル / NIC タイムスタンプ（`Timebase` の追加分を使う）
