# クイックスタート — ソースから SD カードまで

## 前提条件

- Linux x86_64 または ARM64 ホスト
- Rust 1.85+（`rustup` 経由）
- `just` コマンドランナー（`cargo install just`）
- SD カードリーダー + microSD（≥ 8 GB）

## 1. クローン

```bash
git clone https://github.com/celestia-island/aris
cd aris
```

## 2. クロスコンパイルのセットアップ

```bash
just setup-cross
```

Rust ターゲット（`aarch64-unknown-linux-musl` など）をインストールし、お使いのディストリビューション向けの GCC ツールチェーン手順を表示します。

## 3. ファームウェアのビルド

```bash
just build-board nanopi-r3s
```

`output/nanopi-r3s/image.img` を生成します。

## 4. SD カードへの書き込み

```bash
just flash-sd /dev/sdX
```

`/dev/sdX` をお使いの SD カードデバイスに置き換えてください（`lsblk` で確認）。

## 5. 起動

SD カードを NanoPi R3S に挿入し、5V USB-C 電源を接続します。

- **シリアルコンソール**：USB-TTL を 3 ピンのデバッグヘッダー（GND/TX/RX）に接続、1500000 ボー、8N1
- **SSH**：起動後、`ssh root@<ip>`（WAN eth0 から DHCP で取得）

## 6. 検証

```bash
# Check aris-core is running (PID 1)
ps aux | grep aris-core

# Check evernight is running
ps aux | grep evernight

# Check device registration with entelecheia
tail -f /var/log/evernight.log
```
