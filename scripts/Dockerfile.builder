# aris image builder — pre-installed tools for SD card image assembly
# Build once: docker build -t aris-builder -f scripts/Dockerfile.builder .
FROM ubuntu:22.04

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update -qq && \
    apt-get install -y -qq --no-install-recommends \
        dosfstools \
        mtools \
        e2fsprogs \
        gdisk \
        u-boot-tools \
        wget \
        ca-certificates \
        build-essential \
        gcc-aarch64-linux-gnu \
        bc \
        bison \
        flex \
        libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Cross-compile static busybox for aarch64
RUN wget -q "https://busybox.net/downloads/busybox-1.36.1.tar.bz2" -O /tmp/bb.tar.bz2 && \
    tar xjf /tmp/bb.tar.bz2 -C /tmp && \
    cd /tmp/busybox-1.36.1 && \
    make ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu- defconfig && \
    sed -i 's/# CONFIG_STATIC is not set/CONFIG_STATIC=y/' .config && \
    make ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu- -j"$(nproc)" 2>&1 | tail -5 && \
    cp busybox /usr/local/bin/busybox-aarch64 && \
    aarch64-linux-gnu-strip /usr/local/bin/busybox-aarch64 && \
    rm -rf /tmp/busybox* /tmp/bb.tar.bz2

WORKDIR /work
