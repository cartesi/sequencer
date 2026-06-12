#!/usr/bin/env bash
# Build watchdog Lua native deps: lcurl (lua-cURLv3) into .deps/lua.
# Sources are fetched at build time (pinned); only UPSTREAM is tracked in git.
# JSON is pure Lua under watchdog/third_party/json.lua (no compile step).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out_dir="${root}/.deps/lua"
out_so="${out_dir}/lcurl.so"
upstream_file="${root}/watchdog/third_party/lua-curl/UPSTREAM"
versions_file="${root}/release/versions.env"

resolve_upstream_sha() {
    if [[ -n "${LUA_CURL_UPSTREAM_SHA:-}" ]]; then
        echo "${LUA_CURL_UPSTREAM_SHA}"
        return
    fi
    if [[ -f "${versions_file}" ]]; then
        # shellcheck disable=SC1090
        local from_versions
        from_versions="$(
            set -a
            source "${versions_file}"
            set +a
            echo "${LUA_CURL_UPSTREAM_SHA:-}"
        )"
        if [[ -n "${from_versions}" ]]; then
            echo "${from_versions}"
            return
        fi
    fi
    if [[ -f "${upstream_file}" ]]; then
        grep -E '^Commit:' "${upstream_file}" | awk '{print $2}'
        return
    fi
}

resolve_tarball_sha256() {
    if [[ -n "${LUA_CURL_TARBALL_SHA256:-}" ]]; then
        echo "${LUA_CURL_TARBALL_SHA256}"
        return
    fi
    if [[ -f "${versions_file}" ]]; then
        # shellcheck disable=SC1090
        local from_versions
        from_versions="$(
            set -a
            source "${versions_file}"
            set +a
            echo "${LUA_CURL_TARBALL_SHA256:-}"
        )"
        if [[ -n "${from_versions}" ]]; then
            echo "${from_versions}"
            return
        fi
    fi
}

upstream_sha="$(resolve_upstream_sha)"
if [[ -z "${upstream_sha}" ]]; then
    echo "watchdog-lua-deps: could not resolve Lua-cURLv3 upstream pin" >&2
    exit 1
fi

tarball_sha256="$(resolve_tarball_sha256)"
if [[ -z "${tarball_sha256}" ]]; then
    echo "watchdog-lua-deps: could not resolve Lua-cURLv3 tarball sha256 pin" >&2
    exit 1
fi

upstream_tar="https://github.com/Lua-cURL/Lua-cURLv3/archive/${upstream_sha}.tar.gz"
src_cache="${root}/.deps/lua-curl-src/${upstream_sha}"

mkdir -p "${out_dir}"

resolve_lua_bin() {
    if [[ -n "${LUA_BIN:-}" ]]; then
        echo "${LUA_BIN}"
        return
    fi
    for bin in lua5.4 lua; do
        if command -v "${bin}" >/dev/null 2>&1; then
            echo "${bin}"
            return
        fi
    done
}

lua_bin="$(resolve_lua_bin || true)"

lcurl_loadable() {
    [[ -n "${lua_bin}" ]] || return 1
    "${lua_bin}" -e "package.cpath='${out_dir}/?.so;'..package.cpath; require('lcurl')" >/dev/null 2>&1
}

if [[ -f "${out_so}" ]] && lcurl_loadable; then
    exit 0
fi

fetch_sources() {
    echo "watchdog-lua-deps: fetching Lua-cURLv3 ${upstream_sha}" >&2
    local tmp archive
    tmp="$(mktemp -d)"
    archive="${tmp}/lua-curl.tar.gz"
    trap 'rm -rf "${tmp}"' RETURN
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "${upstream_tar}" -o "${archive}"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "${archive}" "${upstream_tar}"
    else
        echo "watchdog-lua-deps: need curl or wget to fetch Lua-cURLv3" >&2
        exit 1
    fi
    echo "${tarball_sha256}  ${archive}" | sha256sum --check
    tar -xzf "${archive}" -C "${tmp}"
    shopt -s nullglob
    local dirs=("${tmp}"/Lua-cURLv3-*)
    if [[ ${#dirs[@]} -ne 1 ]]; then
        echo "watchdog-lua-deps: unexpected Lua-cURLv3 extract layout" >&2
        exit 1
    fi
    rm -rf "${src_cache}"
    mkdir -p "$(dirname "${src_cache}")"
    cp -a "${dirs[0]}" "${src_cache}"
}

if [[ ! -f "${src_cache}/Makefile" ]]; then
    fetch_sources
fi

lua_inc="${LUA_INC:-}"
if [[ -z "${lua_inc}" ]]; then
for dir in /usr/include/lua5.4 /usr/include/lua5.3 /usr/include/lua; do
    if [[ -f "${dir}/lua.h" ]]; then
        lua_inc="${dir}"
        break
    fi
done
fi

if [[ -z "${lua_inc}" ]]; then
    echo "watchdog-lua-deps: install Lua headers (e.g. lua5.4-dev)" >&2
    exit 1
fi

if ! command -v make >/dev/null 2>&1; then
    echo "watchdog-lua-deps: install make" >&2
    exit 1
fi

if ! pkg-config --exists libcurl 2>/dev/null; then
    echo "watchdog-lua-deps: install libcurl dev package (libcurl4-openssl-dev or similar)" >&2
    exit 1
fi

# Lua-cURL Makefile uses LUA_INC (not LUA_INCLUDE_DIR). On Debian/Ubuntu headers
# live under /usr/include/lua5.4/, not /usr/include/.
lua_impl=""
if pkg-config --exists lua5.4 2>/dev/null; then
    lua_impl="lua5.4"
elif pkg-config --exists lua5.3 2>/dev/null; then
    lua_impl="lua5.3"
fi

make_args=(
    "LUA_INC=${lua_inc}"
    "CURL_LIBS=$(pkg-config --libs libcurl)"
)
if [[ -z "${lua_impl}" && "${lua_inc}" == *lua5.4* ]]; then
    lua_impl="lua5.4"
elif [[ -z "${lua_impl}" && "${lua_inc}" == *lua5.3* ]]; then
    lua_impl="lua5.3"
fi
if [[ -n "${lua_impl}" ]]; then
    make_args+=("LUA_IMPL=${lua_impl}")
fi

make -C "${src_cache}" "${make_args[@]}" >/dev/null

built_so="$(find "${src_cache}" -name 'lcurl.so' -o -name 'cURL.so' | head -1)"
if [[ -z "${built_so}" ]]; then
    echo "watchdog-lua-deps: make succeeded but lcurl.so not found" >&2
    exit 1
fi
cp "${built_so}" "${out_so}"

if [[ -z "${lua_bin}" ]]; then
    echo "watchdog-lua-deps: install lua5.4 (or set LUA_BIN) to verify lcurl.so" >&2
    exit 1
fi

if ! lcurl_loadable; then
    echo "watchdog-lua-deps: built lcurl.so but ${lua_bin} cannot load it (Lua version mismatch?)" >&2
    exit 1
fi
