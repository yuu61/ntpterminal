# ntpterminal

NTP サーバに 1 秒未満の間隔で問い合わせ、サーバ時刻を大きな数字で表示する TUI（Rust）。Linux 推奨、Windows でも動く。

## コマンド

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
sh scripts/privileged_tests.sh   # Linux のみ: raw ソケット / AF_PACKET を使う確認（root 不要）
```

実機のサーバがなくても `cargo run -- sim-server` と `cargo run -- tui 127.0.0.1:12300` で動かせる。
手順の詳細は `docs/testing.md`。

## 守ること

- 層は `domain`（I/O なし）← `application`（ユースケース・ポート）← `infrastructure` / `presentation`。
  組み立ては `src/main.rs` だけ。OS 固有のコードと `unsafe` は `infrastructure` だけに置く。
- `domain` は時計を読まず、時刻は引数で受け取る。`time_sync` から `packet_crafting` を参照しない（翻訳は `application::acl`）。
- 時刻の 3 種類を混ぜない: サーバ時刻 `NtpTimestamp`、モノトニック `PcTime`（delay・基準点）、
  壁時計 `LocalStamp.wall`（offset の計算だけ）。カーネルなどの時刻は transport の中でモノトニックに写してから渡す。
- 使えない指定の組み合わせは黙って無視せずエラーにする。実際に使えた方式を表示で偽らない。
- テストは実時間を待たない（偽の `ClockPort` と模擬 `PacketPort`）。送受信の機能は、成功したことだけでなく
  カウンタや実際に送られたパケットで中身を確かめる。
- 依存は使うときにルートの `[workspace.dependencies]` へ追加する。エラーはライブラリが `thiserror`、`main.rs` だけ `anyhow`。
- コメントと UI の文言は日本語で書く。
- 公開 NTP サーバに短い間隔で問い合わせない（手動の確認でも）。

各部分の決まりごととその理由は、モジュール先頭のドキュメントコメントに書いてある
（例: `transport/raw_ip.rs`、`transport/link.rs`、`transport/kernel_ts.rs`、`tui/big_digits.rs`）。
