# 빠른 시작 — 소스부터 SD 카드까지

## 사전 요구 사항

- Linux x86_64 또는 ARM64 호스트
- Rust 1.85+ (`rustup` 통해)
- `just` 명령 실행기 (`cargo install just`)
- SD 카드 리더 + microSD (≥ 8 GB)

## 1. 클론

```bash
git clone https://github.com/celestia-island/aris
cd aris
```

## 2. 크로스 컴파일 설정

```bash
just setup-cross
```

Rust 타겟 (`aarch64-unknown-linux-musl` 등)을 설치하고, 사용 중인 배포판에 맞는 GCC 툴체인 지침을 출력합니다.

## 3. 펌웨어 빌드

```bash
just build-board nanopi-r3s
```

`output/nanopi-r3s/image.img`를 생성합니다.

## 4. SD 카드에 쓰기

```bash
just flash-sd /dev/sdX
```

`/dev/sdX`를 SD 카드 디바이스로 교체하세요 (`lsblk`로 확인).

## 5. 부팅

SD 카드를 NanoPi R3S에 삽입하고, 5V USB-C 전원을 연결합니다.

- **시리얼 콘솔**: USB-TTL을 3핀 디버그 헤더 (GND/TX/RX)에 연결, 1500000 보, 8N1
- **SSH**: 부팅 후, `ssh root@<ip>` (WAN eth0에서 DHCP로 획득)

## 6. 확인

```bash
# Check aris-core is running (PID 1)
ps aux | grep aris-core

# Check evernight is running
ps aux | grep evernight

# Check device registration with entelecheia
tail -f /var/log/evernight.log
```
