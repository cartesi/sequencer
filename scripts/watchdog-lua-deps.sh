#!/usr/bin/env bash
# Build watchdog Lua native deps: lcurl (lua-cURLv3) into .deps/lua.
# JSON is pure Lua under watchdog/third_party/json.lua (no compile step).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out_dir="${root}/.deps/lua"
out_so="${out_dir}/lcurl.so"
vendor_dir="${root}/watchdog/third_party/lua-curl"
upstream_sha="9f8b6dba8b5ef1b26309a571ae75cda4034279e5"
upstream_tar="https://github.com/Lua-cURL/Lua-cURLv3/archive/${upstream_sha}.tar.gz"

mkdir -p "${out_dir}"

lcurl_loadable() {
    lua -e "package.cpath='${out_dir}/?.so;'..package.cpath; require('lcurl')" >/dev/null 2>&1
}

if [[ -f "${out_so}" ]] && lcurl_loadable; then
    exit 0
fi

if [[ ! -f "${vendor_dir}/Makefile" ]]; then
    echo "watchdog-lua-deps: populating ${vendor_dir} from pinned Lua-cURLv3 (${upstream_sha})" >&2
    tmp="$(mktemp -d)"
    trap 'rm -rf "${tmp}"' EXIT
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "${upstream_tar}" | tar -xz -C "${tmp}"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO- "${upstream_tar}" | tar -xz -C "${tmp}"
    else
        echo "watchdog-lua-deps: need curl or wget to fetch Lua-cURLv3" >&2
        exit 1
    fi
    shopt -s nullglob
    dirs=("${tmp}"/Lua-cURLv3-*)
    if [[ ${#dirs[@]} -ne 1 ]]; then
        echo "watchdog-lua-deps: unexpected Lua-cURLv3 extract layout" >&2
        exit 1
    fi
    rm -rf "${vendor_dir}"
    mkdir -p "$(dirname "${vendor_dir}")"
    cp -a "${dirs[0]}" "${vendor_dir}"
fi

lua_inc=""
for dir in /usr/include/lua5.4 /usr/include/lua5.3 /usr/include/lua; do
    if [[ -f "${dir}/lua.h" ]]; then
        lua_inc="${dir}"
        break
    fi
done

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

build_dir="$(mktemp -d)"
trap 'rm -rf "${build_dir}"' EXIT
cp -a "${vendor_dir}/." "${build_dir}/"
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
if [[ -n "${lua_impl}" ]]; then
    make_args+=("LUA_IMPL=${lua_impl}")
fi

make -C "${build_dir}" "${make_args[@]}" >/dev/null

built_so="$(find "${build_dir}" -name 'lcurl.so' -o -name 'cURL.so' | head -1)"
if [[ -z "${built_so}" ]]; then
    echo "watchdog-lua-deps: make succeeded but lcurl.so not found" >&2
    exit 1
fi
cp "${built_so}" "${out_so}"

if ! lcurl_loadable; then
    echo "watchdog-lua-deps: built lcurl.so but lua cannot load it (Lua version mismatch?)" >&2
    exit 1
fi
