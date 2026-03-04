# Stage 1: download `rollup-init`
# -----------------------------------------------------------------------------
FROM riscv64/debian:stable-slim AS extractor

ARG MACHINE_EMULATOR_TOOLS_VERSION=0.17.2
ARG TOOLS_SHA512="4af9911a5a76738d526bfc2b5462cf96c9dee98ec8b23f3ca91ac4849d5761765f471b5e2e8779809bc4a26d2799f8e744622864fa549ada5941e21d999ff4be"

ADD https://github.com/cartesi/machine-guest-tools/releases/download/v${MACHINE_EMULATOR_TOOLS_VERSION}/machine-guest-tools_riscv64.deb /tmp/tools.deb

RUN echo "${TOOLS_SHA512}  /tmp/tools.deb" | sha512sum -c - \
    && dpkg -x /tmp/tools.deb /tmp/out


# Stage 2: rootfs
# -----------------------------------------------------------------------------
# alpine:3.22.2 (riscv64/alpine)
FROM riscv64/alpine@sha256:372839ff152f938e12282226fb5f9ddaef72f9662dcadbf9dd0de5ce287c694e

# Add libgcc for Rust
RUN apk add --no-cache libgcc

# Copy `cartesi-init`
COPY --from=extractor --chmod=755 /tmp/out/usr/sbin/cartesi-init /usr/sbin/cartesi-init

RUN adduser -h /dapp -D dapp
ENV PATH="/dapp:${PATH}"
WORKDIR /dapp
COPY --chown=dapp:dapp --chmod=755 out/dapp .

# ENTRYPOINT ["dapp"]
# CMD ["dapp"]
