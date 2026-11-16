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
cargo run -- tui 192.168.1.123 --footer "Sponsored by ..."   # 大きい時計（q / Esc / Ctrl+C で終了）
cargo run -- once 192.168.1.123
cargo run -- once 192.168.1.123 --json
cargo run -- stats 192.168.1.123 --interval-ms 500 --count 20
cargo run -- sim-server --offset-ms -0.978      # 実機がないとき。別の端末で ↓
cargo run -- stats 127.0.0.1:12300

# raw-ip モード（Linux・root）: IPv4 / UDP ヘッダを自作して送る
sudo ntpterminal tui 192.168.1.123 --transport raw-ip --dscp 46 --ttl 64
#   ヘッダ指定: --ttl / --dscp / --ecn / --ip-id / --no-df / --udp-checksum compute|zero|0xNNNN
#   送信元ポート: --source-port（udp / raw-ip / link 共通）

# link モード（Linux・root）: Ethernet フレームから自作して送る（IP ヘッダはカーネルに書き換えられない）
sudo ntpterminal tui 192.168.1.123 --transport link --vlan 10 --pcp 5 --dscp 46
#   Ethernet の指定: --dst-mac（省略時は近隣表か ARP）/ --vlan / --pcp、IP の指定: --ip-checksum 0xNNNN

# タイムスタンプ（Linux）: auto（既定）/ kernel / user
ntpterminal stats 192.168.1.123 --timestamping kernel
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
- `Anchor` は必ず Monotonic。**カーネル / NIC の時刻は transport の中でモノトニック時計に写してから**ドメインに渡す
  （`infrastructure::clock::rebase_kernel_time`）。ドメインは常にモノトニック時計の時刻だけを見る。
  `Timebase` の Monotonic 以外の値は予約（ドメインの安全網として残している）。
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
2. **M2（済）**: TUI（`tui` サブコマンド）… 大きい数字・情報欄。Sparkline は任意（未実装）。
3. **M3（済）**: `raw-ip` モード（Linux）… IPv4 / UDP ヘッダの生成（`packet_crafting::ipv4_udp`）。
   送信方式は TUI・`once`・`stats` に表示する。
4. M4: カーネルのタイムスタンプ（**済**・Linux）/ `link` モード（**済**・Linux）/ NIC（ハードウェア）の
   タイムスタンプ（未実装。`ethtool -T` で全パケットの受信に時刻を付けられる NIC がないと検証できない）

## raw-ip モードの決まりごと

- Linux のみ（`#[cfg(target_os = "linux")]`）。ほかの OS では `--transport raw-ip` を「Linux のみ対応」エラーにする。
  socket2 は Linux のときだけ依存に入る（`[target.'cfg(target_os = "linux")'.dependencies]`）。
- IPv4 のサーバだけ。root か CAP_NET_RAW が必要（なければ分かるエラーを出す）。
- **Linux のカーネルが書き換える項目は約束しない**: IP ヘッダのチェックサムと全長は常に上書き、
  Identification が 0 ならカーネルが値を入れる。これらを自由にできるのは link モード（M4）。
- 送信元 IP は自分で入れる（ガード用 UDP ソケットの `local_addr()`）。UDP チェックサムの疑似ヘッダに使うため。
  送信元 IP を偽る指定は作らない。
- 受信用 raw ソケットには届いた UDP がすべて来る（自分の送信の写しも）。送信元・宛先が一致するものだけを
  transport の中で通し、期限は 1 回の recv につき最初に 1 回だけ決める。
- 指定できない組み合わせは黙って無視しない（UDP モードでのヘッダ指定はエラー）。

## link モードの決まりごと

- Linux のみ。AF_PACKET（root か CAP_NET_RAW）。Ethernet とループバックのインターフェースだけ（tun などは分かるエラー）。
- カーネルは IP ヘッダに手を加えない: Identification 0 も、`--ip-checksum` の固定値もそのまま届く。
  `--ip-checksum` / `--dst-mac` / `--vlan` / `--pcp` を udp / raw-ip で指定したらエラー（`transport::validate`）。
- インターフェースは「サーバに connect した UDP ソケットの送信元 IP」を持つものを使う（getifaddrs）。
- 宛先 MAC: `--dst-mac` → 近隣表（/proc/net/arp、ATF_COM のものだけ）→ 自前の ARP 要求。
  次の転送先は /proc/net/route の最長一致・メトリック最小（ポリシールーティングには非対応）。
  **VLAN を使うときは ARP 要求にも同じタグを付け、近隣表は見ない。**
- AF_PACKET のソケットはプロトコル 0 で作ってから、インターフェースと EtherType に bind する。
  プロトコルの値はネットワークバイト順（`link_addr` の中で変換）。
- 受信では自分の送信の写し（PACKET_OUTGOING）を捨て、送信元・宛先で絞り込む。BPF は使っていない。
- `sockaddr_ll` を作る `unsafe` は `transport/link/interface.rs` の `link_addr` だけ。
  `clippy::undocumented_unsafe_blocks` を deny にしているので、`unsafe` には必ず `// SAFETY:` を書く。
- カーネルの送信時刻は AF_PACKET では `SOL_PACKET` / `PACKET_TX_TIMESTAMP` の制御メッセージで届く
  （nix は解釈しないので `kernel_ts::send_timestamp_id` で読む）。

### raw ソケット・AF_PACKET のテスト

`crates/infrastructure/tests/raw_ip.rs` は `#[ignore]`。WSL / Linux で、ビルドは通常ユーザ、実行は
ネットワーク名前空間の中で行う（root 不要・ホストのネットワークに影響しない）:

```sh
BIN=$(cargo test -p ntpterminal-infrastructure --test raw_ip --no-run 2>&1 | grep -oE "/[^ )]+raw_ip-[0-9a-f]+" | head -1)
unshare -rn sh -c "ip link set lo up && $BIN --ignored --test-threads=1"
# link のテスト（crates/infrastructure/tests/link.rs）はループバックで自分あてに送るので、
# 名前空間の中でだけ lo の route_localnet と accept_local を 1 にする（AF_PACKET から入れたフレームは
# 受信側の経路判定を通り、127.0.0.1 あて・自分が送信元のものは既定で捨てられるため）
unshare -rn sh -c "ip link set lo up && echo 1 > /proc/sys/net/ipv4/conf/lo/route_localnet \
  && echo 1 > /proc/sys/net/ipv4/conf/lo/accept_local && $BIN --ignored --test-threads=1"
```

ループバックでは UDP チェックサムが検証されないことがあるので、テストでは送信したパケットを横から
受け取ってチェックサムを計算し直している。

実際の MAC・自前の ARP・VLAN タグ（PCP）は、veth でつないだ 2 つの名前空間で確かめる（root 不要）:

```sh
sh scripts/link_veth_check.sh <ntpterminal のパス>   # 「すべて OK」と出れば成功
```

## タイムスタンプの決まりごと（M4）

- `--timestamping auto|kernel|user`（既定 auto）。カーネルのタイムスタンプは Linux のみ・権限不要（SO_TIMESTAMPING、ソフトウェア）。
  Linux 以外で `kernel` を指定したらエラー、`auto` はユーザ空間に戻る。
- 実装は `infrastructure/src/transport/kernel_ts.rs`。UDP と raw-ip の両方で使う（raw-ip は受信用・送信用で別ソケット）。
- **T1 と T4 の出どころをそろえる**: 送信時刻（T1）がカーネルから取れなかった問い合わせでは、
  受信（T4）もユーザ空間の時刻にする。片方だけカーネルの時刻だと offset が片道の遅れの半分ほど偏る。
  例外: T1 がカーネルの時刻なのに受信時刻が取れない（まれ）ときは、T1 を後から変えられないので混在になる。
- 送信時刻はエラーキュー（MSG_ERRQUEUE）から読む。OPT_ID の番号で照合し、古い送信の時刻を使わない。
  エラーキューは毎回空にする（溜まると受信バッファを圧迫する）。3 回続けて取れなければ送信時刻の取得を
  setsockopt ごとやめる。MSG_CTRUNC の制御メッセージは「時刻なし」として扱う。
- 実際に使えた時刻の出どころは `application::timestamping::TimestampingStatus` に記録し、TUI・stats・once に出す
  （「カーネル」と表示したまま実はユーザ空間、を起こさない）。
- テスト: `crates/infrastructure/tests/kernel_timestamps.rs`（Linux・通常のテスト）はカウンタで
  「T1 と T4 の両方がカーネルの時刻だった」ことを確かめる。成功しただけ・offset が合っただけでは確かめない。
- 32 bit の Linux（32 bit 版 Raspberry Pi OS など）では制御メッセージの時刻の形式が違う可能性があるので、使う前に確認する。

## TUI の決まりごと

- ratatui はバージョンを固定し、crossterm は単独で入れず `ratatui::crossterm` を使う。
- 同期は別スレッドの `SyncService::run_until` で回し、`SyncSnapshot` をチャネルで UI に渡す（集約を共有しない）。
- 同期スレッドと UI には **同じ `ClockPort` インスタンス**を渡す（モノトニック時計の起点がずれると黙って狂う）。
- 大きい時計の表示は `domain::time_sync::DisplayClock` を通す。基準点が切り替わっても**逆戻りさせない**
  （ずれは経過時間の 50% までの速さで吸収。1 秒を超えるずれのときだけ一度に合わせる）。
- 配色は 16 色の名前付き ANSI カラーだけ（RGB は Linux コンソールで出ないことがある）。
- 端末の「曖昧幅（East Asian Ambiguous）」は半角（多くの端末の既定）を前提にする。
  大きい数字は全点灯セルを背景色付きの空白で描くので、倍率が偶数なら曖昧幅の文字を使わない。
  奇数倍率の `▀` / `▄` と状態の `●` は曖昧幅。
- レイアウト計算は `u16` の飽和演算（`saturating_*`）で行う（端末を縮めたときのパニック防止）。

### TUI を自動で確認する（WSL + tmux）

Windows には tmux がないので、WSL（Ubuntu）でビルドして tmux の中で動かす。
大きい数字は背景色付きの空白なので、`capture-pane -e` の色情報から読み取る必要がある。

```sh
wsl -e bash -lc 'cd /mnt/c/repository/ntpterminal && CARGO_TARGET_DIR=$HOME/.cache/ntpterminal-target cargo build'
# B=$HOME/.cache/ntpterminal-target/debug/ntpterminal
# tmux new-session -d -s ntp -x 120 -y 40 -n sim "$B sim-server --offset-ms -0.978 --refid GNSS"
# tmux new-window -t ntp -n tui "$B tui 127.0.0.1:12300"
# tmux capture-pane -t ntp:tui -p        # 「同期済み」が出たら準備完了
# tmux send-keys -t ntp:tui q; tmux kill-session -t ntp
```
