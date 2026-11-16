# ntpterminal

GNSS タイムサーバのデモ展示用の NTP 時計である。ローカルの NTP サーバに 1 秒未満の間隔で問い合わせ、
サーバの時刻を端末いっぱいに大きな数字で表示する。Rust で書かれた TUI アプリケーションである。

- PC の時計がずれていても、正しいサーバ時刻を表示する
- オフセットや遅延などの測定値に加え、タイムサーバの GNSS 受信機の状態（SNMP）も表示できる
- 送信方式と、タイムスタンプの取得方法（カーネル、NIC のハードウェア）を選択できる

## ビルド

Linux を推奨する（Windows でも動作するが、一部の機能は使えない）。ビルドには Rust 1.88 以上が必要である。

```sh
cargo build --release     # target/release/ntpterminal が生成される
```

## 試してみる

付属の疑似サーバを使えば、実機の NTP サーバがなくても動作を確認できる。

```sh
cargo run -- sim-server --offset-ms -0.978 --refid GNSS   # 疑似 NTP サーバを 127.0.0.1:12300 で起動
cargo run -- tui 127.0.0.1:12300                          # 別の端末で実行する。q で終了
```

公開 NTP サーバには使わないこと（[理由](docs/usage.md#公開-ntp-サーバでは使わないこと)）。

## ドキュメント

| ファイル                                             | 内容                                                         |
|------------------------------------------------------|--------------------------------------------------------------|
| [docs/usage.md](docs/usage.md)                       | 使い方（各コマンド、送信方式とタイムスタンプ、SNMP）         |
| [docs/configuration.md](docs/configuration.md)       | コマンドライン引数と設定ファイルの書き方、全項目の一覧       |
| [docs/config.example.toml](docs/config.example.toml) | 設定ファイルのひな形（全項目入り）                           |
| [docs/architecture.md](docs/architecture.md)         | 全体の構成（レイヤー、ドメインの分割、時刻の扱い、スレッド） |
| [docs/testing.md](docs/testing.md)                   | 手動での確認手順（権限が必要なテスト、TUI、SNMP、NIC の時計）|
| [CLAUDE.md](CLAUDE.md)                               | 開発用のコマンドと、コードを書くときの決まり                 |
