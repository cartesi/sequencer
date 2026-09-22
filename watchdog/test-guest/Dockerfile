# Rootfs for the watchdog test guest. Mirrors examples/canonical-app/Dockerfile;
# keep the guest-tools and Alpine pins in step with it.

# Stage 1: download `cartesi-init`
# -----------------------------------------------------------------------------
FROM riscv64/debian:stable-slim AS extractor

ARG MACHINE_EMULATOR_TOOLS_VERSION=0.18.0
ARG TOOLS_SHA512="11a48b68545818a5e3b978e8c5339284ea7ac84acc44cc5b16550839c99066aaeec3c4107e043e653597613ebfb24fe0748984888af841341ce5bcb4dd01dae5"

ADD https://github.com/cartesi/machine-guest-tools/releases/download/v${MACHINE_EMULATOR_TOOLS_VERSION}/machine-guest-tools_riscv64.deb /tmp/tools.deb

RUN echo "${TOOLS_SHA512}  /tmp/tools.deb" | sha512sum -c - \
    && dpkg -x /tmp/tools.deb /tmp/out


# Stage 2: rootfs
# -----------------------------------------------------------------------------
# alpine:3.22.2 (riscv64/alpine)
FROM riscv64/alpine@sha256:372839ff152f938e12282226fb5f9ddaef72f9662dcadbf9dd0de5ce287c694e

ARG ALPINE_MAIN_REPOSITORY=https://dl-cdn.alpinelinux.org/alpine/v3.22/main
ARG LIBGCC_VERSION=14.2.0-r6

# Add libgcc for Rust
RUN apk add --no-cache \
    --repository="${ALPINE_MAIN_REPOSITORY}" \
    "libgcc=${LIBGCC_VERSION}"

# Only `cartesi-init` is taken from the guest tools: it is a shell script. Their
# `nvram` and `xhalt` are glibc binaries, so musl replacements stand in.
COPY --from=extractor --chmod=755 /tmp/out/usr/sbin/cartesi-init /usr/sbin/cartesi-init
COPY --chmod=755 nvram.sh /usr/bin/nvram
COPY --chmod=755 out/xhalt /usr/sbin/xhalt

RUN adduser -h /dapp -D dapp
ENV PATH="/dapp:${PATH}"
WORKDIR /dapp
COPY --chown=dapp:dapp --chmod=755 out/dapp .
