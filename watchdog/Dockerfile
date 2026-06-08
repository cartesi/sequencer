# Watchdog compare-mode runtime image.
# Build args RELEASE_TAG, GIT_COMMIT, and cartesi-machine pins must match release/versions.env
# and the canonical-machine-image-* tarballs produced in the same release.

ARG DEBIAN_VERSION=bookworm-slim
FROM debian:${DEBIAN_VERSION} AS build

ARG CARTESI_MACHINE_VERSION
ARG CARTESI_MACHINE_DEB_SHA256
ARG TARGETARCH
ARG LUA_CURL_UPSTREAM_SHA

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    wget \
    make \
    gcc \
    pkg-config \
    liblua5.4-dev \
    libcurl4-openssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

COPY scripts/watchdog-lua-deps.sh scripts/watchdog-lua-deps.sh
COPY watchdog/third_party/lua-curl watchdog/third_party/lua-curl
COPY watchdog/third_party/json.lua watchdog/third_party/json.lua

ENV LUA_CURL_UPSTREAM_SHA=${LUA_CURL_UPSTREAM_SHA}
RUN bash scripts/watchdog-lua-deps.sh

RUN set -euo pipefail \
    && case "${TARGETARCH}" in \
      amd64) CM_ARCH=amd64 ;; \
      arm64) CM_ARCH=arm64 ;; \
      *) echo "unsupported TARGETARCH=${TARGETARCH}" >&2; exit 1 ;; \
    esac \
    && wget -O /tmp/machine-emulator.deb \
      "https://github.com/cartesi/machine-emulator/releases/download/${CARTESI_MACHINE_VERSION}/machine-emulator_${CM_ARCH}.deb" \
    && echo "${CARTESI_MACHINE_DEB_SHA256}  /tmp/machine-emulator.deb" | sha256sum --check \
    && dpkg-deb -x /tmp/machine-emulator.deb /tmp/cm \
    && mkdir -p /out/bin /out/lib /out/share/cartesi-machine \
    && cp -a /tmp/cm/usr/bin/cartesi-machine /out/bin/ \
    && cp -a /tmp/cm/usr/share/cartesi-machine/. /out/share/cartesi-machine/ \
    && cp /build/.deps/lua/lcurl.so /out/lib/lcurl.so

FROM debian:${DEBIAN_VERSION}

ARG RELEASE_TAG
ARG GIT_COMMIT
ARG CARTESI_MACHINE_VERSION
ARG LUA_CURL_UPSTREAM_SHA

LABEL org.opencontainers.image.title="sequencer-watchdog" \
      org.opencontainers.image.version="${RELEASE_TAG}" \
      org.opencontainers.image.revision="${GIT_COMMIT}" \
      org.cartesi.sequencer.release-tag="${RELEASE_TAG}" \
      org.cartesi.cartesi-machine.version="${CARTESI_MACHINE_VERSION}" \
      org.cartesi.lua-curl.upstream-sha="${LUA_CURL_UPSTREAM_SHA}"

RUN apt-get update && apt-get install -y --no-install-recommends \
    lua5.4 \
    libcurl4 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /out/bin/cartesi-machine /usr/local/bin/cartesi-machine
COPY --from=build /out/share/cartesi-machine /usr/local/share/cartesi-machine
COPY --from=build /out/lib/lcurl.so /opt/watchdog/lib/lcurl.so

COPY watchdog /opt/watchdog/lua/watchdog
COPY watchdog/third_party/json.lua /opt/watchdog/lua/watchdog/third_party/json.lua
COPY watchdog/docker-entrypoint.sh /usr/local/bin/watchdog

RUN chmod +x /usr/local/bin/watchdog /usr/local/bin/cartesi-machine \
    && printf '%s\n' \
      "{" \
      "  \"component\": \"sequencer-watchdog\"," \
      "  \"release_tag\": \"${RELEASE_TAG}\"," \
      "  \"git_commit\": \"${GIT_COMMIT}\"," \
      "  \"cartesi_machine_version\": \"${CARTESI_MACHINE_VERSION}\"," \
      "  \"lua_curl_upstream_sha\": \"${LUA_CURL_UPSTREAM_SHA}\"" \
      "}" > /opt/watchdog/RELEASE.json

ENV WATCHDOG_LUA_DEPS=/opt/watchdog/lib \
    LUA_PATH="/opt/watchdog/lua/?.lua;/opt/watchdog/lua/?/init.lua;;" \
    PATH="/usr/local/share/cartesi-machine:${PATH}"

WORKDIR /opt/watchdog
ENTRYPOINT ["/usr/local/bin/watchdog"]
CMD []
