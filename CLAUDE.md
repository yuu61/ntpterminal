# ntpterminal

NTP サーバに 1 秒未満の間隔で問い合わせ、サーバ時刻を大きな数字で表示する TUI（Rust）。Linux 推奨、Windows でも動く。
使い方は `docs/usage.md`、全体の構成（レイヤー・時刻の扱い・スレッド）は `docs/architecture.md` にある。
使い方や構成を変えたら、これらも更新する。

## コマンド

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings   # 設定: Cargo.toml の [workspace.lints]、clippy.toml
cargo doc --workspace --no-deps                         # rustdoc の lint（CI は -D warnings）
cargo fmt --all                                         # 設定: rustfmt.toml
shellcheck scripts/*.sh                                 # WSL / CI（Windows には入っていない）。設定: .shellcheckrc
sh scripts/privileged_tests.sh   # Linux のみ: raw ソケット / AF_PACKET を使う確認（root 不要）
```

lint は clippy（restriction を含む全グループ）・rustc・rustdoc・shellcheck のすべてを有効にしている。
警告はコードを直して消す。外してよいのは Cargo.toml に書いた基準 [1]〜[4] に当てはまる lint だけで、
コードの中で外すのはやむを得ない場合だけ `#[expect(lint, reason = "…")]` を使う（`#[allow]` はエラーになる）。
clippy.toml はルートと `crates/domain/` の 2 つ（マージされないので、共通の設定は両方に書く）。
`cfg(target_os = "linux")` のコードは Windows の clippy では検査されない。WSL か CI（ubuntu と windows）で確かめる。

実機のサーバがなくても `cargo run -- sim-server` と `cargo run -- tui 127.0.0.1:12300` で動かせる。
手順の詳細は `docs/testing.md`。

オプションは TOML の設定ファイルにも書ける（`--config <PATH>`、省略時はカレントディレクトリの `config.toml`。
キーは長いオプションと同じ名前。決まりは `src/config.rs` の先頭）。スクリプトからは `--config /dev/null` で手元の設定を読ませない。
オプションを足したら `src/config.rs`、全項目の例 `docs/config.example.toml`、項目一覧 `docs/configuration.md` にも足す
（足し忘れはテストで落ちる）。既定値を変えたときは `docs/configuration.md` の表も直す。

## 守ること

- 層は `domain`（I/O なし）← `application`（ユースケース・ポート）← `infrastructure` / `presentation`。
  組み立ては `src/main.rs` だけ。OS 固有のコードと `unsafe` は `infrastructure` だけに置く。
- `domain` は時計を読まず、時刻は引数で受け取る。`time_sync` から `packet_crafting` を、`receiver` から `snmp` を、
  `ptp_clock` から `ptp_management`・`ptp_message` を参照しない（翻訳は `application::acl`）。
- 時刻の 3 種類を混ぜない: サーバ時刻 `NtpTimestamp`、モノトニック `PcTime`（delay・基準点）、
  壁時計 `LocalStamp.wall`（offset の計算だけ）。カーネルや PHC の時刻は infrastructure（transport・`phc.rs`）の中で
  モノトニックに写してから渡す。
- 使えない指定の組み合わせは黙って無視せずエラーにする。実際に使えた方式を表示で偽らない。
- テストは実時間を待たない（偽の `ClockPort` と模擬 `PacketPort`）。送受信の機能は、成功したことだけでなく
  カウンタや実際に送られたパケットで中身を確かめる。
- 依存は使うときにルートの `[workspace.dependencies]` へ追加する。エラーはライブラリが `thiserror`、`main.rs` だけ `anyhow`。
- コメントと UI の文言は日本語で書く。
- 公開 NTP サーバに短い間隔で問い合わせない（手動の確認でも）。

各部分の決まりごととその理由は、モジュール先頭のドキュメントコメントに書いてある
（例: `transport/raw_ip.rs`、`transport/link.rs`、`transport/kernel_ts.rs`、`transport/hardware_ts.rs`、`tui/big_digits.rs`、
GNSS 受信機を SNMP で読む部分は `acl/mib.rs` と `usecase/receiver_monitor.rs`、
ptp4l が合わせる PHC は `ptp_clock/source.rs`・`usecase/ptp4l_client.rs`・`usecase/phc_selection.rs`・`phc.rs`・`ptp4l.rs`、
PTP のスレーブ（`--ptp`）は `ptp_clock/master.rs`・`usecase/ptp_slave.rs`・`transport/ptp_udp.rs`・`sim_ptp_master.rs`）。
