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
tmux capture-pane -t ntp:tui -p     # 「Synchronized」が出たら準備完了
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

情報欄の Ref ID の下に GNSS receiver と Satellites の 2 行が出る（`--snmp-community` を付けたときだけ）。
MIB は自動で判定する。`tui --snmp-mib` で固定もでき、固定した MIB が応答しなければ、ほかの MIB に切り替えずに
エラーを出す。疑似エージェントの衛星の数は 20 秒ごとに変わる。microchip-s650 では、衛星が多いと一覧が
128 バイトで切れて衛星系の内訳が出なくなる（実機と同じ）。疑似エージェントは net-snmp でも読める:
`snmpwalk -v2c -c public 127.0.0.1:16100 .1.3.6.1`

実機に問い合わせるときは、機器の SNMP の設定（v2c を有効にする・コミュニティ名・問い合わせを許すアドレス）を
確かめておく。対応している MIB と、メーカーごとの衛星の数の意味は `crates/application/src/acl/mib.rs` の先頭にある。

## 推移のグラフ

情報欄の下の `Last N samples`（推移のグラフ）は、`tui --trend` を付けたときだけ出す（既定では出さない）。
付けても、出して時計の倍率が変わらない大きさの端末でだけ出る。120×40 なら出るが、80×24 では出ない。

## QR コード

`tui --qr bottom-right` などで、このリポジトリの URL の QR コードを隅に出す。隅がほかの表示と重なると出ないので、
120×40 以上の端末で推移なしにして確かめる。スマートフォンのカメラで読み取り、`https://github.com/yuu61/ntpterminal`
が開くかを見る（テストで確かめているのは、描いたセルから読み戻したモジュールが QR の生成結果と一致するところまで）。

```sh
cargo run -- sim-server
cargo run -- --config /dev/null tui 127.0.0.1:12300 --qr bottom-right   # 別の端末で
```

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
「ptp4l が合わせている PHC がありません。…はソフトウェアのタイムスタンプ（-S）です」のエラーになる（それが正しい）。WSL2 には Hyper-V の PHC
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

- `free_running 1` の ptp4l は、ハードウェアのタイムスタンプでも PHC の番号を -1 と知らせる（PHC を合わせない）ので、
  「nic0 は PHC の番号が -1 です」のエラーになる。PHC の表示を確かめるときは付けない
- chrony が同じ NIC のハードウェアのタイムスタンプ（`hwtimestamp`）を使っているときは、ptp4l の既定の設定だと
  受信の設定が PTP だけになり、chrony が NTP の受信時刻を取れなくなる。ptp4l の設定に `hwts_filter check`
  （設定を変えずに、いまの設定で足りるか確かめるだけ。`all` なら足りる）を書く
- マスタのいない LAN で `clientOnly 1`・`network_transport L2` にすると、ptp4l は LISTENING のまま何も送らない。
  この状態でも PHC は読むので、`once`（終了コード 1）・`stats`・`tui`（大きな時計は `--:--:--.---`）の表示と、
  ptp4l が知らせる PHC・読み方（`once`・`stats` では「同時読み取り」、`tui` では `simultaneous read`）を確かめられる
  （2026-11 に I219-V・linuxptp 4.2 で確認）

## PTP のマスタに問い合わせる（`--ptp`）

同期処理（マスタの選び方・`Delay_Req` の間隔と中身・応答の照合・`once` の失敗の理由）は、偽の時計と模擬マスタを使うテスト
（`crates/application/tests/ptp_slave.rs`）で、UDP の送受信は疑似マスタとループバックのテスト
（`crates/infrastructure/tests/ptp_loopback.rs`。root 不要・全 OS）で確かめている。メッセージの解析は、ptp4l が実際に
送ったバイト列でも確かめている（`ptp_message/message.rs` の `decodes_messages_captured_from_ptp4l`）。

### 疑似マスタで試す（root 不要）

```sh
cargo run -- sim-ptp-master --port 12319 --to 127.0.0.1:13319 --interface 127.0.0.1 --offset-ms 2.5 --log-sync-interval -2
cargo run -- tui --ptp --ptp-port 13319 --ptp-interface 127.0.0.1 --ptp-master 127.0.0.1:12319 --trend   # 別の端末で
```

マスタを選ぶまで（Announce 2 通）1 秒ほど `Looking for master (domain 0)` と出て、その後 `Synchronized` になり、
PC offset（PC との差）が +2.5 ms 前後になる。疑似マスタは `Delay_Resp` に最小間隔を入れない（ユニキャストの応答は
ptp4l と同じく「指定なし」）ので、Intervals は `Delay_Req 1 s (default minimum)` になる
（`once --ptp` では「間隔」が `Delay_Req 1 秒（既定の最小間隔）`）。

### 本物の ptp4l のマスタ（Linux・root。LAN には何も流れない）

2 つのネットワーク名前空間を veth でつなぎ、片方でソフトウェアのタイムスタンプの ptp4l をマスタ専用で動かして、
もう片方から問い合わせる。2 つの名前空間は同じ `CLOCK_REALTIME` を使うので、PC との差はほぼ 0 になる。

```sh
sudo ip netns add ptp-m && sudo ip netns add ptp-s
sudo ip link add ptp-vm type veth peer name ptp-vs
sudo ip link set ptp-vm netns ptp-m && sudo ip link set ptp-vs netns ptp-s
sudo ip -n ptp-m addr add 10.99.0.1/24 dev ptp-vm && sudo ip -n ptp-s addr add 10.99.0.2/24 dev ptp-vs
sudo ip -n ptp-m link set ptp-vm up && sudo ip -n ptp-s link set ptp-vs up
# m.cfg: [global] に serverOnly 1・time_stamping software・logSyncInterval -1・logMinDelayReqInterval -1
sudo ip netns exec ptp-m ptp4l -f m.cfg -i ptp-vm -m &
sudo ip netns exec ptp-s target/debug/ntpterminal --config /dev/null once --ptp --ptp-interface ptp-vs --timestamping kernel
sudo ip netns exec ptp-s target/debug/ntpterminal --config /dev/null once --ptp --ptp-interface ptp-vs --ptp-master 10.99.0.1
sudo ip netns exec ptp-s target/debug/ntpterminal --config /dev/null stats --ptp --ptp-interface ptp-vs --count 20 --interval-ms 250
sudo ip netns exec ptp-s target/debug/ntpterminal --config /dev/null once --ptp --ptp-interface ptp-vs --ptp-domain 24   # ドメイン違い
sudo ip netns del ptp-m && sudo ip netns del ptp-s
```

確かめること（2026-11 に WSL2・linuxptp 4.0 で確認）:

- 状態が「同期済み」、PC との差が ±0.001 ms 以内、経路遅延が数百 ns（veth・カーネルの時刻）、タイムスタンプが
  「カーネル 20 回 / ユーザ空間 0 回（送信時刻なし 0 回）」
- ソフトウェアのタイムスタンプの ptp4l は時刻系を ARB とし（UTC をそのまま送る）、「UTC との差」は「換算なし（時刻系が ARB）」
- グランドマスタ・clockClass（248）・時刻源（内部の発振器）が、マスタの側の `pmc 'GET DEFAULT_DATA_SET'` などと合う
- `--ptp-master` を付けると、ptp4l もユニキャストで `Delay_Resp` を返し、測れる
- ドメインが違うと、届いているドメイン番号を添えて「PTP のマスタが見つかりません」で終了コード 1
- root なしでは、319 番を使えない理由（root か `CAP_NET_BIND_SERVICE`）を添えてエラーになる

### LAN の別のホストのマスタ（Windows のマルチキャスト）

LAN の Linux のホストで疑似マスタを動かし、別の PC から問い合わせる。そのホストが NTP にも応えるなら、NTP で測った
差と見比べられる（PC の時計がずれていても、両方に同じだけ出る）。

```sh
sudo ntpterminal --config /dev/null sim-ptp-master --interface <マスタのホストの IPv4> --domain 99   # マスタのホストで
ntpterminal --config /dev/null stats <マスタのホスト> --count 6                                      # 以下は PC で（NTP）
ntpterminal --config /dev/null once --ptp --ptp-interface <PC の IPv4>                               # ドメイン違い
ntpterminal --config /dev/null stats --ptp --ptp-domain 99 --ptp-interface <PC の IPv4> --count 20
ntpterminal --config /dev/null stats --ptp --ptp-domain 99 --ptp-interface <PC の IPv4> --ptp-master <マスタのホスト> --count 10
```

確かめること（2026-11 に Windows 11（1 GbE・ユーザ空間の時刻）と Proxmox のホストで確認）:

- PC との差が NTP の offset と 0.2 ms 以内で合う（実測は −15.6 ms 前後で、差は 0.15 ms 以内）。経路遅延は 250〜300 µs、
  成功/試行は 20/20。疑似マスタは TAI で送るので、UTC に換算できていなければ 37 秒ずれる
- ドメインを付けないと、「ドメイン 99 のメッセージが届いています」と添えて終了コード 1
- `--ptp-master` を付けても（ユニキャストの `Delay_Req`）、同じ差になる

Windows では、319・320 番で待ち受けるとファイアウォールの確認が出る。許可しないと ntpterminal.exe への受信を
止めるルールが作られ、「マスタが見つかりません」（ほかのドメインのヒントもない）になる。そのときは管理者の
PowerShell で、止めるルールを消して許可のルールを作る（確認が終わったら `Remove-NetFirewallRule` で消す）。

```powershell
Remove-NetFirewallRule -DisplayName ntpterminal
New-NetFirewallRule -DisplayName "ntpterminal PTP" -Direction Inbound -Action Allow -Profile Private -Protocol UDP -LocalPort 319,320 -Program <ntpterminal.exe のフルパス>
```

ptp4l を LAN のマスタにするときは、ptp4l が UDP のソケットをインターフェースに縛る（`SO_BINDTODEVICE`）ことに気を付ける。

- ブリッジ（Proxmox の vmbr0 など）の下の NIC で動かすと、受け取ったパケットはブリッジに渡るので、`Delay_Req` が
  ptp4l に届かない。ブリッジ自体はソフトウェアの送信時刻に対応していないので、ptp4l が起動しない
- IPv4 アドレスのない NIC で動かすと、Announce・Sync は送れるが、受け取った `Delay_Req` は逆経路フィルタ
  （`rp_filter`）で捨てられる（`nstat -az TcpExtIPReversePathFilter` が増える）。PC の側は「`Delay_Resp` が届きません」になる
- どちらの場合も、ptp4l の Announce・Sync・`Follow_Up` を PC が LAN のマルチキャストで受け取れること（「マスタの時刻を
  測っています」になる）までは確かめられる。疑似マスタは全アドレスに bind するので、ブリッジでもそのまま使える

### ハードウェアのタイムスタンプの ptp4l のマスタ（ブリッジの下の NIC）

ブリッジの下の NIC でも、その NIC に届いた PTP の UDP だけをブリッジに渡さず（nftables の broute）、NIC に /32 の
アドレスを付ければ（経路は変わらない）、ptp4l をマスタにできる。どちらも一時的な変更なので、終わったら消す。
chrony が同じ NIC で `hwtimestamp` を使っていると、受信フィルタが PTP でなくなり `hwts_filter check` の ptp4l が
起動しないので、chrony の側を `hwtimestamp <NIC> rxfilter ptp` にする。

```sh
sudo nft -f - <<'EOF'
table bridge ntpterminal_ptp {
	chain prerouting {
		type filter hook prerouting priority -300; policy accept;
		iifname "<NIC>" ether type ip udp dport { 319, 320 } meta broute set 1
	}
}
EOF
sudo ip addr add <使っていない IPv4>/32 dev <NIC>
# hw.cfg: [global] に serverOnly 1・time_stamping hardware・hwts_filter check・free_running 1・domainNumber 99・logAnnounceInterval 0
sudo ptp4l -f hw.cfg -i <NIC> -m
sudo phc_ctl <NIC> cmp      # 「offset from CLOCK_REALTIME」は REALTIME − PHC
sudo nft delete table bridge ntpterminal_ptp && sudo ip addr del <IPv4>/32 dev <NIC>   # 終わったら
```

確かめること（2026-11 に X710（i40e）・linuxptp 4.2 のマスタと、Windows 11 のスレーブで確認）:

- ptp4l は合わせていない PHC を TAI として送る（時刻系 PTP・UTC との差 37・「未確定」）。PC との差は
  −（REALTIME − PHC）− 37 秒 + NTP の offset になる（実測は PHC が約 11.8 時間遅れていて、差は 0.15 ms 以内で合った。
  換算しなければ 37 秒ずれる）。マスタの時刻も PHC − 37 秒で出る
- マルチキャストでも `--ptp-master <付けたアドレス>` でも 20/20・10/10 で測れ、ptp4l の送信時刻のエラーはない
  （`ethtool -S <NIC>` の `tx_hwtstamp_timeouts`・`skipped` も 0）

### 実機のマスタ

GNSS で合わせたグランドマスタなど、時刻を合わせた LAN の PTP マスタへの問い合わせは、まだ確かめていない。
確かめるときは `sudo ntpterminal tui --ptp --ptp-interface eth0` で状態・グランドマスタ・clockClass・UTC との差を
マスタの表示と見比べ、PC の時計を chrony などで合わせていれば PC との差が小さいことを見る。
NIC のハードウェアのタイムスタンプ（`--timestamping hardware`）には、まだ対応していない。
