# Watchdog one-cycle runtime image.
# Build args RELEASE_TAG, GIT_COMMIT, and cartesi-machine pins must match toolchain-pins.env
# and the canonical-machine-image-* tarballs produced in the same release.

# cartesi-machine v0.20.x libs require glibc >= 2.38 (bookworm is 2.36).
ARG DEBIAN_VERSION=trixie-slim
FROM debian:${DEBIAN_VERSION} AS build

SHELL ["/bin/bash", "-o", "pipefail", "-c"]

ARG CARTESI_MACHINE_VERSION
ARG CARTESI_MACHINE_DEB_SHA256_AMD64
ARG CARTESI_MACHINE_DEB_SHA256_ARM64
ARG TARGETARCH

# wget + ca-certificates fetch the cartesi-machine .deb; gcc/pkg-config + Lua
# headers compile the vendored lcurl.so. lua-curl is vendored in-tree under
# watchdog/third_party/lua-curl/src, so the build needs no network for it.
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    wget \
    gcc \
    pkg-config \
    lua5.4 \
    liblua5.4-dev \
    libcurl4-openssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

COPY scripts/watchdog-lua-deps.sh scripts/watchdog-lua-deps.sh
COPY watchdog/third_party watchdog/third_party

RUN bash scripts/watchdog-lua-deps.sh

RUN case "${TARGETARCH}" in \
      amd64) DEB_SHA="${CARTESI_MACHINE_DEB_SHA256_AMD64}"; CM_ARCH=amd64 ;; \
      arm64) DEB_SHA="${CARTESI_MACHINE_DEB_SHA256_ARM64}"; CM_ARCH=arm64 ;; \
      *) echo "unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac \
    && wget -O /tmp/machine-emulator.deb \
      "https://github.com/cartesi/machine-emulator/releases/download/${CARTESI_MACHINE_VERSION}/machine-emulator_${CM_ARCH}.deb" \
    && echo "${DEB_SHA}  /tmp/machine-emulator.deb" | sha256sum --check \
    && dpkg-deb -x /tmp/machine-emulator.deb /tmp/cm \
    && mkdir -p /out/bin /out/lib /out/lib/lua/5.4 /out/share/cartesi-machine /out/share/lua/5.4 \
    && cp -a /tmp/cm/usr/bin/cartesi-machine /out/bin/ \
    && cp -a /tmp/cm/usr/share/cartesi-machine/. /out/share/cartesi-machine/ \
    && cp -a /tmp/cm/usr/lib/lua/5.4/. /out/lib/lua/5.4/ \
    && cp -a /tmp/cm/usr/share/lua/5.4/. /out/share/lua/5.4/ \
    && cp /tmp/cm/usr/lib/libcartesi*.so /out/lib/ \
    && cp /build/.deps/lua/lcurl.so /out/lib/lcurl.so

FROM debian:${DEBIAN_VERSION}

ARG RELEASE_TAG
ARG GIT_COMMIT
ARG CARTESI_MACHINE_VERSION

LABEL org.opencontainers.image.title="sequencer-watchdog" \
      org.opencontainers.image.description="Cartesi sequencer watchdog (compare daemon)" \
      org.opencontainers.image.source="https://github.com/cartesi/sequencer" \
      org.opencontainers.image.version="${RELEASE_TAG}" \
      org.opencontainers.image.revision="${GIT_COMMIT}" \
      org.cartesi.sequencer.release-tag="${RELEASE_TAG}" \
      org.cartesi.cartesi-machine.version="${CARTESI_MACHINE_VERSION}"

RUN apt-get update && apt-get install -y --no-install-recommends \
    lua5.4 \
    libcurl4 \
    libslirp0 \
    libgomp1 \
    ca-certificates \
    util-linux \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /out/bin/cartesi-machine /usr/local/bin/cartesi-machine
COPY --from=build /out/share/cartesi-machine /usr/local/share/cartesi-machine
COPY --from=build /out/lib/lcurl.so /opt/watchdog/lib/lcurl.so
COPY --from=build /out/lib/lua/5.4/ /usr/local/lib/lua/5.4/
COPY --from=build /out/share/lua/5.4/ /usr/local/share/lua/5.4/
COPY --from=build /out/share/lua/5.4/ /usr/share/lua/5.4/
COPY --from=build /out/lib/libcartesi*.so /usr/local/lib/

COPY watchdog /opt/watchdog/lua/watchdog
COPY watchdog/third_party/json.lua /opt/watchdog/lua/watchdog/third_party/json.lua
# Shared cross-stack fixtures (watchdog/tests/run.lua expects repo-root tests/fixtures/).
COPY tests/fixtures /opt/watchdog/lua/tests/fixtures
COPY watchdog/sequencer-watchdog /usr/local/bin/sequencer-watchdog

RUN chmod +x /usr/local/bin/sequencer-watchdog /usr/local/bin/cartesi-machine \
    && printf '%s\n' \
      "{" \
      "  \"component\": \"sequencer-watchdog\"," \
      "  \"release_tag\": \"${RELEASE_TAG}\"," \
      "  \"git_commit\": \"${GIT_COMMIT}\"," \
      "  \"cartesi_machine_version\": \"${CARTESI_MACHINE_VERSION}\"" \
      "}" > /opt/watchdog/RELEASE.json

ENV WATCHDOG_LUA_DEPS=/opt/watchdog/lib \
    LUA_PATH="/opt/watchdog/lua/?.lua;/opt/watchdog/lua/?/init.lua;/usr/local/share/lua/5.4/?.lua;/usr/local/share/lua/5.4/?/init.lua;;" \
    LUA_CPATH="/opt/watchdog/lib/?.so;/usr/local/lib/lua/5.4/?.so;;" \
    LD_LIBRARY_PATH="/usr/local/lib" \
    PATH="/usr/local/share/cartesi-machine:${PATH}"

WORKDIR /opt/watchdog
ENTRYPOINT ["/usr/local/bin/sequencer-watchdog"]
CMD []
