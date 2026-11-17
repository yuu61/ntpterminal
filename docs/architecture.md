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
| `ptp_management`  | PTP の管理メッセージ（ptp4l の管理用ソケットで使う）の組み立てと解析              |
| `ptp_message`     | PTP のメッセージ（Announce・Sync・`Follow_Up`・`Delay_Req`・`Delay_Resp`）の組み立てと解析 |
| `ptp_clock`       | PTP の時計を時刻源として扱う。集約ルートは、ptp4l が合わせる PHC の `PhcSource` と、スレーブとして問い合わせるマスタの `MasterSource`。ptp4l の状態とマスタの情報のモデルもここにある |
| `shared_kernel`   | 複数のコンテキストで共有する時刻関連の型（`NtpTimestamp`、`TimeDelta`、`PcTime` など） |

`packet_crafting` は試験のために規格外のパケットも表現できるが、`time_sync` は検証済みの値しか扱わない。
同様に、`snmp` は BER の読み書きだけを担当して値の意味には関知せず、`receiver` は SNMP について何も知らない。
`ptp_management`・`ptp_message` と `ptp_clock` の関係も同じである。
こうした性質の違うモデルが混ざらないよう、`time_sync` から `packet_crafting` を、`receiver` から `snmp` を、
`ptp_clock` から `ptp_management`・`ptp_message` を参照することを禁じている。両者の間の変換は application 層の ACL
（[`acl.rs`](../crates/application/src/acl.rs)）が担い、メーカー固有の MIB の解釈も ACL（[`acl/mib.rs`](../crates/application/src/acl/mib.rs)）で、
ptp4l のデータセットの解釈も ACL（[`acl/ptp4l.rs`](../crates/application/src/acl/ptp4l.rs)）で、PTP のメッセージの解釈も
ACL（[`acl/ptp_message.rs`](../crates/application/src/acl/ptp_message.rs)）で行う。
`ptp_clock` は、基準点を作る処理（`time_sync` の `ClockTracker`）を `time_sync` から使う。

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
2. 新しい測定結果が、それまでの基準点と同じ時間の流れに乗っているかを確かめる。遅延の半分と時計のずれの見込みを
   超えて食い違えば保留し、次の測定結果もそれと合えば時刻の段差（サーバの時刻の修正・閏秒・PC のスリープ）とみなして、
   それまでの測定結果を捨てる（[`time_sync/continuity.rs`](../crates/domain/src/time_sync/continuity.rs)）
3. 直近の測定結果のうち、「遅延の半分 + 古さ × 15 ppm」が最も小さいものを選ぶ。NTP のクロックフィルタと同じ考え方で、
   古い測定結果ほど、そこから時刻を進める間に時計の進み方の差でずれることを見込む
   （[`time_sync/filter.rs`](../crates/domain/src/time_sync/filter.rs)）
4. 選んだ測定結果から、「モノトニック時計がこの値のとき、サーバ時刻はこの値だった」という基準点（アンカー）を作る
   （[`time_sync/anchor.rs`](../crates/domain/src/time_sync/anchor.rs)）
5. 基準点から、モノトニック時計の経過時間だけ時刻を進めて表示する。PC のモノトニック時計とサーバの時計の進み方の差は、
   1 分以上の測定結果から推定して補正する（自走中もこの補正で進め続ける。
   [`time_sync/frequency.rs`](../crates/domain/src/time_sync/frequency.rs)）。基準点が切り替わったときに生じるずれは、
   表示が逆戻りしないよう少しずつ吸収する（[`time_sync/display_clock.rs`](../crates/domain/src/time_sync/display_clock.rs)）

応答の検証（mode、Originate Timestamp の照合、KoD、LI など）と、同期状態（未同期・同期済み・自走・拒否）の遷移は
[`time_sync/source.rs`](../crates/domain/src/time_sync/source.rs) で定義している。
2〜5 の処理（段差の確認・クロックフィルタ・周波数の推定・基準点）は [`time_sync/tracker.rs`](../crates/domain/src/time_sync/tracker.rs) の
`ClockTracker` にまとめてあり、ptp4l が合わせる PHC と PTP のマスタでも同じものを使う。

### ptp4l が合わせる PHC（`--ptp4l`）

PHC の読み取りは、前後に読んだ PC の時刻を T1・T4、PHC の時刻（TAI から UTC に直したもの）を T2 = T3 とする標本にする。
delay は読み取りの前後の幅、offset は PHC − PC の壁時計になる。この標本を NTP と同じ `ClockTracker` に入れて基準点を作る。
同期の状態は ptp4l のポートの状態から決め、ptp4l が同期を知らせるまでは基準点を表示に出さない
（[`ptp_clock/source.rs`](../crates/domain/src/ptp_clock/source.rs)）。カーネルの時刻と同じく、PHC の読み取りは
infrastructure の中でモノトニック時計の時刻に写してから渡す（[`phc.rs`](../crates/infrastructure/src/phc.rs)）。

### PTP のマスタ（`--ptp`）

PTP の 1 回の交換（マスタの Sync と、PC の `Delay_Req`）の 4 つの時刻から標本を作る。Sync の送信（t1）・受信（t2）、
`Delay_Req` の送信（t3）・マスタの受信（t4）を、T3 = t1・T4 = t2・T1 = t3・T2 = t4 として持つ。送受信の向きが NTP と
逆なので、`Sample::new` の順序の確認はせず、同じ式で offset と delay を求める（`Sample::from_ptp_exchange`）。
基準点は Sync を受け取った瞬間に、マスタ時刻 t1 + 往復遅延 / 2 を対応させる。`Delay_Req` は Sync を受け取った直後に送り、
その間の PC とマスタの時計の進み方の差が遅延に入らないようにする。マスタの選び方・`Delay_Req` の間隔・時刻の出どころの
そろえ方は [`ptp_clock/master.rs`](../crates/domain/src/ptp_clock/master.rs) の先頭にある。

## スレッド構成（tui）

| スレッド        | 処理                                                       | UI への受け渡し                       |
|-----------------|------------------------------------------------------------|---------------------------------------|
| メイン          | TUI のイベントループ（約 60 fps で再描画）                 | ―                                     |
| `ntp-sync`      | `SyncService`（NTP の問い合わせを続ける）                  | `SyncSnapshot` をチャネルで送る       |
| `ptp4l-phc`     | `PhcService`（`--ptp4l` のとき、`ntp-sync` の代わり。ptp4l の状態と PHC を読み続ける） | `PhcSnapshot` をチャネルで送る |
| `ptp-slave`     | `PtpSlaveService`（`--ptp` のとき、`ntp-sync` の代わり。マスタのメッセージを受け取り、`Delay_Req` を送る） | `MasterSnapshot` をチャネルで送る |
| `ptp-event`・`ptp-general` | PTP の 2 つの UDP ポートの受信（`--ptp` のとき。受け取った直後に時計を読み、`ptp-slave` にチャネルで渡す） | ― |
| `snmp-receiver` | `ReceiverMonitor`（`--snmp-community` を指定したときのみ） | `ReceiverSnapshot` をチャネルで送る   |

UI は集約に直接触れず、届いたスナップショットを描画するだけである。同期と受信機のスレッドと UI は、同じ時計インスタンスを共有する。
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
| [`ptp_udp.rs`](../crates/infrastructure/src/transport/ptp_udp.rs)         | PTP のスレーブの送受信（UDP の 319・320 番、マルチキャスト、カーネルのタイムスタンプ） |

`udp.rs` と `ptp_udp.rs` 以外は Linux 専用（`cfg(target_os = "linux")`）のため、Windows 上の clippy では検査されない
（`ptp_udp.rs` も、カーネルのタイムスタンプとインターフェース名の部分は Linux 専用）。

ptp4l が合わせる PHC の表示に使う口は transport の外にある（どちらも Linux 専用）。

| モジュール                                                     | 内容                                                                 |
|----------------------------------------------------------------|----------------------------------------------------------------------|
| [`phc.rs`](../crates/infrastructure/src/phc.rs)                | PHC（`/dev/ptpN`）とシステムの時計の組での読み取り。`hardware_ts.rs` も使う |
| [`ptp4l.rs`](../crates/infrastructure/src/ptp4l.rs)            | ptp4l の管理用ソケット（Unix ドメインソケット）への送受信と、PHC を開く入口 |
どの実装を使うか、またどのオプションの組み合わせをエラーとするかは [`transport.rs`](../crates/infrastructure/src/transport.rs) で決めている。
タイムスタンプを実際にどこから取得できたかは `application::timestamping` のカウンタに記録し、画面に表示する。

## テスト

| 対象                     | 場所                                                          | 実行方法                         |
|--------------------------|---------------------------------------------------------------|----------------------------------|
| 各モジュールの単体テスト | 各ファイルの `mod tests`                                      | `cargo test --workspace`         |
| 同期処理・受信機の監視   | `crates/application/tests/`（偽の時計と模擬ネットワークを使用） | `cargo test --workspace`         |
| UDP の送受信             | `crates/infrastructure/tests/loopback.rs`                     | `cargo test --workspace`         |
| ptp4l と PHC の同期処理  | `crates/application/tests/phc_service.rs`（模擬 ptp4l と模擬 PHC） | `cargo test --workspace`     |
| ptp4l の管理用ソケット   | `crates/infrastructure/tests/ptp4l_socket.rs`（Linux のみ）    | `cargo test --workspace`         |
| 本物の ptp4l と PHC      | `crates/infrastructure/tests/ptp4l_real.rs`（`#[ignore]`）     | [testing.md](testing.md#本物の-ptp4l-と-phclinuxroot) |
| PTP のスレーブの同期処理 | `crates/application/tests/ptp_slave.rs`（偽の時計と模擬マスタ） | `cargo test --workspace`         |
| PTP の UDP の送受信      | `crates/infrastructure/tests/ptp_loopback.rs`（疑似マスタとループバック） | `cargo test --workspace` |
| 本物の ptp4l のマスタ    | 手動（netns で ptp4l のマスタに問い合わせる）                  | [testing.md](testing.md#ptp-のマスタに問い合わせる--ptp) |
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
| ptp4l への問い合わせ（送り元・boundaryHops・ドメイン番号） | [`usecase/ptp4l_client.rs`](../crates/application/src/usecase/ptp4l_client.rs) |
| 表示する PHC の決め方（仮想 PHC、古い linuxptp）     | [`usecase/phc_selection.rs`](../crates/application/src/usecase/phc_selection.rs) |
| PHC の時刻源の状態と、時計を出す条件                 | [`ptp_clock/source.rs`](../crates/domain/src/ptp_clock/source.rs)          |
| PHC の読み方（PRECISE・EXTENDED・基本の ioctl）      | [`phc.rs`](../crates/infrastructure/src/phc.rs)                            |
| PTP のマスタの選び方・`Delay_Req` の間隔・時刻の出どころ | [`ptp_clock/master.rs`](../crates/domain/src/ptp_clock/master.rs)       |
| PTP の送受信（マルチキャスト・ポート・受信のスレッド） | [`transport/ptp_udp.rs`](../crates/infrastructure/src/transport/ptp_udp.rs) |
| 対応している MIB と、衛星数の意味の違い              | [`acl/mib.rs`](../crates/application/src/acl/mib.rs)                        |
| 大きな数字の描画方法（ブロック文字を避ける理由）     | [`tui/big_digits.rs`](../crates/presentation/src/tui/big_digits.rs)         |
| 推移のグラフ、受信機の行、配色                       | [`tui/trend.rs`](../crates/presentation/src/tui/trend.rs)、[`tui/receiver.rs`](../crates/presentation/src/tui/receiver.rs)、[`tui/theme.rs`](../crates/presentation/src/tui/theme.rs) |
| lint を無効にしてよい基準                            | [`Cargo.toml`](../Cargo.toml) の `[workspace.lints]`                        |
