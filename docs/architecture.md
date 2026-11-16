# アーキテクチャ

このドキュメントでは、ntpterminal の全体の構成と、各部分のコードがどこにあるかを説明する。
個々の設計上の決まりとその理由は、リンク先のモジュール先頭にあるドキュメントコメントに書かれているので、
ここでは繰り返さない。ドキュメントコメントは
`cargo doc --workspace --no-deps --document-private-items --open` を実行するとブラウザで読める。

## レイヤーとクレート

```text
domain  ←  application  ←  infrastructure
                        ←  presentation
           （src/main.rs がすべてを組み立てる）
```

矢印は依存の向きを表す（右のレイヤーが左のレイヤーに依存する）。

| レイヤー       | 場所                    | 役割                                                                 |
|----------------|-------------------------|----------------------------------------------------------------------|
| domain         | `crates/domain`         | I/O を持たない純粋なモデル。時計は読まず、時刻はすべて引数で受け取る |
| application    | `crates/application`    | ユースケースと、外側のレイヤーが実装するポート（trait）。コンテキスト間の変換を行う ACL もここにある |
| infrastructure | `crates/infrastructure` | ポートの実装（ソケット、時計、疑似サーバ）。OS 固有のコードと `unsafe` はこのクレートにしか置かない |
| presentation   | `crates/presentation`   | CLI（`once`、`stats`）と TUI                                          |
| 組み立て       | `src/main.rs`           | オプションと設定ファイル（`src/config.rs`）を読み込み、各部品をつなぐ。ロジックは持たない |

domain と application は I/O を持たないため、時刻同期の振る舞いは偽の時計と模擬ネットワークを使って、
実時間を待たずにテストできる（`crates/application/tests/`）。

## domain のコンテキスト

| モジュール        | 内容                                                                              |
|-------------------|-----------------------------------------------------------------------------------|
| `time_sync`       | 時刻同期（コアドメイン）。集約ルートは `TimeSource`。応答の検証、クロックフィルタ、同期状態を扱う |
| `packet_crafting` | NTP メッセージ、IPv4 / UDP、Ethernet、ARP の組み立てと解析。規格外の値も表現できる |
| `snmp`            | SNMP v2c メッセージと BER の相互変換                                              |
| `receiver`        | GNSS 受信機の状態をメーカーに依存しない形で表すモデル                             |
| `shared_kernel`   | 複数のコンテキストで共有する時刻関連の型（`NtpTimestamp`、`TimeDelta`、`PcTime` など） |

`packet_crafting` は試験のために規格外のパケットも表現できるが、`time_sync` は検証済みの値しか扱わない。
同様に、`snmp` は BER の読み書きだけを担当して値の意味には関知せず、`receiver` は SNMP について何も知らない。
こうした性質の違うモデルが混ざらないよう、`time_sync` から `packet_crafting` を、`receiver` から `snmp` を
参照することを禁じている。両者の間の変換は application 層の ACL（[`acl.rs`](../crates/application/src/acl.rs)）が担い、
メーカー固有の MIB の解釈も ACL（[`acl/mib.rs`](../crates/application/src/acl/mib.rs)）で行う。

## 3 種類の時刻

| 種類                   | 型                | 用途                                       |
|------------------------|-------------------|--------------------------------------------|
| サーバ時刻             | `NtpTimestamp`    | T2・T3、および画面に表示する時刻           |
| PC のモノトニック時計  | `PcTime`          | 遅延の計算と、表示する時計を進めるための基準 |
| PC の壁時計（システム時刻） | `LocalStamp.wall` | オフセットの計算のみ                     |

システム時刻は時刻合わせによって進んだり戻ったりするため、遅延の計算と時計の表示にはモノトニック時計を使う。
カーネルや NIC が付けたタイムスタンプは、transport の中でモノトニック時計の値に変換してから domain に渡す。
詳しくは [`shared_kernel.rs`](../crates/domain/src/shared_kernel.rs) を参照。

## 表示する時刻の求め方

1. 問い合わせのたびに、T1〜T4 からオフセット（壁時計を使用）と遅延（モノトニック時計を使用）を求める
   （[`time_sync/sample.rs`](../crates/domain/src/time_sync/sample.rs)）
2. 直近の測定結果のうち、遅延が最も小さいものを選ぶ。NTP のクロックフィルタと同じ考え方である
   （[`time_sync/filter.rs`](../crates/domain/src/time_sync/filter.rs)）
3. 選んだ測定結果から、「モノトニック時計がこの値のとき、サーバ時刻はこの値だった」という基準点（アンカー）を作る
   （[`time_sync/anchor.rs`](../crates/domain/src/time_sync/anchor.rs)）
4. 基準点から、モノトニック時計の経過時間だけ時刻を進めて表示する。基準点が切り替わったときに生じるずれは、
   表示が逆戻りしないよう少しずつ吸収する（[`time_sync/display_clock.rs`](../crates/domain/src/time_sync/display_clock.rs)）

応答の検証（mode、Originate Timestamp の照合、KoD、LI など）と、同期状態（未同期・同期済み・自走・拒否）の遷移は
[`time_sync/source.rs`](../crates/domain/src/time_sync/source.rs) で定義している。

## スレッド構成（tui）

| スレッド        | 処理                                                       | UI への受け渡し                       |
|-----------------|------------------------------------------------------------|---------------------------------------|
| メイン          | TUI のイベントループ（約 60 fps で再描画）                 | ―                                     |
| `ntp-sync`      | `SyncService`（NTP の問い合わせを続ける）                  | `SyncSnapshot` をチャネルで送る       |
| `snmp-receiver` | `ReceiverMonitor`（`--snmp-community` を指定したときのみ） | `ReceiverSnapshot` をチャネルで送る   |

UI は集約に直接触れず、届いたスナップショットを描画するだけである。3 つのスレッドは同じ時計インスタンスを共有する。
モノトニック時計の起点がスレッドごとに異なると、基準点からの計算がエラーにならないまま狂ってしまうためである。
`once` と `stats` はメインスレッドだけで動作する。

## 送受信（infrastructure の transport）

| モジュール                                                                | 内容                                                   |
|---------------------------------------------------------------------------|--------------------------------------------------------|
| [`udp.rs`](../crates/infrastructure/src/transport/udp.rs)                 | OS の UDP ソケットによる送受信（すべての OS）          |
| [`raw_ip.rs`](../crates/infrastructure/src/transport/raw_ip.rs)           | IPv4 / UDP ヘッダを自前で組み立て、raw ソケットで送る  |
| [`link.rs`](../crates/infrastructure/src/transport/link.rs)               | Ethernet フレームから自前で組み立て、`AF_PACKET` で送る |
| [`kernel_ts.rs`](../crates/infrastructure/src/transport/kernel_ts.rs)     | カーネルのタイムスタンプ（`SO_TIMESTAMPING`）          |
| [`hardware_ts.rs`](../crates/infrastructure/src/transport/hardware_ts.rs) | NIC のハードウェアタイムスタンプと、NIC の設定の変更・復元 |

`udp.rs` 以外は Linux 専用（`cfg(target_os = "linux")`）のため、Windows 上の clippy では検査されない。
どの実装を使うか、またどのオプションの組み合わせをエラーとするかは [`transport.rs`](../crates/infrastructure/src/transport.rs) で決めている。
タイムスタンプを実際にどこから取得できたかは `application::timestamping` のカウンタに記録し、画面に表示する。

## テスト

| 対象                     | 場所                                                          | 実行方法                         |
|--------------------------|---------------------------------------------------------------|----------------------------------|
| 各モジュールの単体テスト | 各ファイルの `mod tests`                                      | `cargo test --workspace`         |
| 同期処理・受信機の監視   | `crates/application/tests/`（偽の時計と模擬ネットワークを使用） | `cargo test --workspace`         |
| UDP の送受信             | `crates/infrastructure/tests/loopback.rs`                     | `cargo test --workspace`         |
| カーネルのタイムスタンプ | `crates/infrastructure/tests/kernel_timestamps.rs`（Linux のみ） | `cargo test --workspace`       |
| raw-ip・link             | `crates/infrastructure/tests/{raw_ip,link}.rs`（`#[ignore]`） | `sh scripts/privileged_tests.sh` |

手動での確認手順（TUI、SNMP、NIC の時計）は [testing.md](testing.md) にまとめてある。

## 詳しい説明の場所

| 知りたいこと                                         | ファイル                                                                    |
|------------------------------------------------------|-----------------------------------------------------------------------------|
| 設定ファイルの仕様（優先順位、記述できないキー）     | [`src/config.rs`](../src/config.rs)                                         |
| 公開サーバへの問い合わせ間隔の制限                   | [`polling_policy.rs`](../crates/application/src/polling_policy.rs)          |
| タイムスタンプの取得状況を画面に表示する理由         | [`timestamping.rs`](../crates/application/src/timestamping.rs)              |
| GNSS 受信機の読み取り（MIB の判定、失敗時の扱い）    | [`usecase/receiver_monitor.rs`](../crates/application/src/usecase/receiver_monitor.rs) |
| 対応している MIB と、衛星数の意味の違い              | [`acl/mib.rs`](../crates/application/src/acl/mib.rs)                        |
| 大きな数字の描画方法（ブロック文字を避ける理由）     | [`tui/big_digits.rs`](../crates/presentation/src/tui/big_digits.rs)         |
| 推移のグラフ、受信機の行、配色                       | [`tui/trend.rs`](../crates/presentation/src/tui/trend.rs)、[`tui/receiver.rs`](../crates/presentation/src/tui/receiver.rs)、[`tui/theme.rs`](../crates/presentation/src/tui/theme.rs) |
| lint を無効にしてよい基準                            | [`Cargo.toml`](../Cargo.toml) の `[workspace.lints]`                        |
