# 確認の手順

## 権限が必要なテスト（Linux）

```sh
sh scripts/privileged_tests.sh
```

raw ソケット / AF_PACKET を使う `#[ignore]` のテストと、veth でつないだ 2 つの名前空間での link モードの確認
（実際の MAC・自前の ARP・VLAN タグ）をまとめて実行する。root 不要で、ホストのネットワークには触らない。
Windows では WSL の中で動かす。CI でも `privileged` ジョブ（ubuntu-24.04）で毎回実行する。

## TUI を端末で確かめる

実機の NTP サーバがなくても、疑似サーバで確かめられる。

```sh
cargo run -- sim-server --offset-ms -0.978 --refid GNSS   # 別の端末で ↓
cargo run -- tui 127.0.0.1:12300                          # q / Esc / Ctrl+C で終了
```

カレントディレクトリに `config.toml` があると、その設定も使われる（引数の指定が優先）。
手元の設定を使わずに確かめるときは `--config /dev/null`（Windows では `--config NUL`）を付ける。

自動で確かめるときは tmux の中で動かす（Windows には tmux がないので WSL で）。

```sh
tmux new-session -d -s ntp -x 120 -y 40 -n sim "$BIN sim-server --offset-ms -0.978 --refid GNSS"
tmux new-window -t ntp -n tui "$BIN tui 127.0.0.1:12300"
tmux capture-pane -t ntp:tui -p     # 「同期済み」が出たら準備完了
tmux send-keys -t ntp:tui q; tmux kill-session -t ntp
```

大きい数字は背景色付きの空白で描いているので、`capture-pane -p` では見えない。
`capture-pane -p -e` で色の情報を含めて取り出し、背景色の付いた空白を数字の点として読む。
色は 256 色の番号で出る（大きい数字は `48;5;14`、推移の棒は `48;5;6`）。

## 時刻の段差と、時計の進み方の補正を確かめる

`stats` で問い合わせている間に、疑似サーバを別のずれで起動し直すと、時刻の段差として扱われる。
段差の後の 2 回目の応答で「時刻の段差を検出: -300.000 ms」のような行が出て、以降はその時刻に合わせる。

```sh
cargo run -- sim-server                                          # 別の端末で。stats を始めたら止めて、↓ で起動し直す
cargo run -- sim-server --offset-ms -300
cargo run -- stats --count 30 --interval-ms 200 127.0.0.1:12300
```

1 分以上問い合わせを続けると、統計の「周波数のずれ」に推定値が出る。疑似サーバは PC の壁時計で答えるので、
この値は PC のモノトニック時計と壁時計の進み方の差になる（Windows では時刻同期で壁時計だけが合わせられるので、
数十 ppm になることがある）。確かめるときは、`Instant` と `SystemTime` を 1 分ほど離して読んだ差と比べる。

## GNSS 受信機の行（SNMP）を確かめる

疑似サーバに `--snmp-mib` を付けると、疑似 SNMP エージェント（既定は 127.0.0.1:16100、コミュニティ名は public）も
起動し、その MIB で GNSS 受信機の値を返す（meinberg-os / meinberg-ltng / meinberg-lt / microchip-s650）。

```sh
cargo run -- sim-server --offset-ms -0.978 --refid GNSS --snmp-mib meinberg-ltng   # 別の端末で ↓
cargo run -- tui 127.0.0.1:12300 --snmp-community public --snmp-port 16100
```

情報欄の「Ref ID」の下に「GNSS 受信機」と「衛星」の 2 行が出る（`--snmp-community` を付けたときだけ）。
MIB は自動で判定する。`tui --snmp-mib` で固定もでき、固定した MIB が応答しなければ、ほかの MIB に切り替えずに
エラーを出す。疑似エージェントの衛星の数は 20 秒ごとに変わる。microchip-s650 では、衛星が多いと一覧が
128 バイトで切れて衛星系の内訳が出なくなる（実機と同じ）。疑似エージェントは net-snmp でも読める:
`snmpwalk -v2c -c public 127.0.0.1:16100 .1.3.6.1`

実機に問い合わせるときは、機器の SNMP の設定（v2c を有効にする・コミュニティ名・問い合わせを許すアドレス）を
確かめておく。対応している MIB と、メーカーごとの衛星の数の意味は `crates/application/src/acl/mib.rs` の先頭にある。

## 推移のグラフ

情報欄の下の「直近 N 回の測定値」（推移のグラフ）は、`tui --trend` を付けたときだけ出す（既定では出さない）。
付けても、出して時計の倍率が変わらない大きさの端末でだけ出る。120×40 なら出るが、80×24 では出ない。

## ハードウェアタイムスタンプ（NIC の時計）

`--timestamping hardware` は NIC の PHC の時刻を使う。CI には PHC を持つ NIC がないので、実機で確かめる
（Linux・root。送信方式は udp / raw-ip / link のどれでも使える）。

```sh
ethtool -T eth0                                  # PHC と、受信フィルタの all（か ntp-all）があるか
ethtool --get-hwtimestamp-cfg eth0               # 起動前の設定（off / none なら ntpterminal が有効にする）
ntpterminal --config /dev/null stats --count 20 --timestamping hardware 192.168.1.123
ethtool --get-hwtimestamp-cfg eth0               # 起動前と同じに戻っていること
```

「タイムスタンプ」の内訳が `ハードウェア 20 回` になり、送信方式に `eth0 の HW 時刻` が出れば使えている。
NIC で無効だったときは `（一時的に有効化）` が付き、終了（Ctrl+C・SIGTERM・SIGHUP・メインスレッドの panic を含む）で
元の設定に戻る。ブリッジ（Proxmox の vmbr0 など）越しでも、下の物理ポートを自動で選ぶ。

e1000e は、NIC の送信が off のままソケットにハードウェアの送信時刻を頼むと、1 秒待って
`clearing Tx timestamp hang` をカーネルのログに出す。確かめた後に `journalctl -k | grep -i hang` と
`ethtool -S eth0 | grep hwtstamp` で増えていないことも見る。決まりごとの詳細は `transport/hardware_ts.rs` の先頭。

## ptp4l が合わせる PHC（`--ptp4l`）

ptp4l とのやりとり・状態の決め方・表示は、模擬 ptp4l と模擬 PHC を使うテスト
（`crates/application/tests/phc_service.rs`）と、実際の Unix ドメインソケットを使うテスト
（`crates/infrastructure/tests/ptp4l_socket.rs`。Linux・root 不要）で確かめている。

### 本物の ptp4l と PHC（Linux・root）

本物の ptp4l に問い合わせる確認と、PHC を読む確認は `#[ignore]` のテスト（`crates/infrastructure/tests/ptp4l_real.rs`）に
してある。ハードウェアのタイムスタンプの NIC がなくても、ネットワーク名前空間を 2 つ veth でつなぎ、ソフトウェアの
タイムスタンプの ptp4l をマスタとスレーブで動かせば、管理メッセージの読み書きは確かめられる（ホストの LAN には何も流れない）。
`free_running 1` で時計を合わせないようにする（合わせないので、スレーブは SLAVE にならず UNCALIBRATED で止まる）。

```sh
sudo ip netns add ptp-m; sudo ip netns add ptp-s
sudo ip link add ptp-vm type veth peer name ptp-vs
sudo ip link set ptp-vm netns ptp-m; sudo ip link set ptp-vs netns ptp-s
sudo ip -n ptp-m addr add 10.99.0.1/24 dev ptp-vm; sudo ip -n ptp-m link set ptp-vm up
sudo ip -n ptp-s addr add 10.99.0.2/24 dev ptp-vs; sudo ip -n ptp-s link set ptp-vs up
# 設定ファイル（どちらも [global] に time_stamping software・free_running 1・domainNumber 24 と、別々の
# uds_address・uds_ro_address を書く。マスタは serverOnly 1、スレーブは clientOnly 1）
sudo ip netns exec ptp-m ptp4l -i ptp-vm -f m.cfg -m &
sudo ip netns exec ptp-s ptp4l -i ptp-vs -f s.cfg -m &
# 管理用ソケットの応答は抽象名前空間の名前（ネットワーク名前空間ごと）に届くので、ptp4l と同じ名前空間で動かす
cargo test -p ntpterminal-infrastructure --test ptp4l_real --no-run   # 表示された実行ファイルを ↓ で動かす
sudo ip netns exec ptp-s env NTPTERMINAL_PTP4L_SOCKET=<スレーブの uds_address> NTPTERMINAL_PTP_DOMAIN=24 \
    <実行ファイル> --ignored --nocapture reads_a_real_ptp4l
sudo ip netns exec ptp-s pmc -u -b 0 -d 24 -s <スレーブの uds_address> 'GET CURRENT_DATA_SET' 'GET PARENT_DATA_SET'
sudo env NTPTERMINAL_PHC=0 <実行ファイル> --ignored --nocapture reads_a_real_phc   # /dev/ptp0 を読む
```

テストが出す値（ポートの状態・マスタとの差・経路遅延・グランドマスタ・clockClass・UTC との差・時刻系）を、
`pmc` の表示と見比べる。ソフトウェアのタイムスタンプの ptp4l は PHC を使わないので、`ntpterminal once --ptp4l` は
「ソフトウェアのタイムスタンプで動いています」のエラーになる（それが正しい）。WSL2 には Hyper-V の PHC
（`/dev/ptp0`）があり、`PTP_SYS_OFFSET_PRECISE` と `_EXTENDED` には対応していないので、`PTP_SYS_OFFSET` で読む。

### 実機（ハードウェアのタイムスタンプの ptp4l）

PHC を使う ptp4l を動かし、`once`・`stats`・`tui` で確かめる。

```sh
sudo ptp4l -i eth0 -m                                  # 別の端末で（ハードウェアのタイムスタンプ）
sudo pmc -u -b 0 'GET PORT_DATA_SET' 'GET TIME_STATUS_NP'
sudo ntpterminal --config /dev/null once --ptp4l
sudo ntpterminal --config /dev/null stats --ptp4l --count 20
sudo ntpterminal --config /dev/null tui --ptp4l --trend
```

`once` の「マスタとの差」「経路遅延」「グランドマスタ」が `pmc` の値と合い、「PC との差」が `phc2sys` などで
PC の時計を合わせていればほぼ 0 になることを確かめる。ptp4l は起動すると NIC のタイムスタンプの設定
（`ethtool --get-hwtimestamp-cfg eth0`）を変え、終了しても戻さない。PTP のマルチキャストのグループにも参加し、
マスタが見つからなければ自分がマスタになって Announce・Sync を送る（`clientOnly 1` で防げる）。
共用のネットワークで試すときは、先に影響を確かめておく。
