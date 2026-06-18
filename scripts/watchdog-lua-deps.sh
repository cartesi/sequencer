#!/usr/bin/env bash
# Build the watchdog's native Lua dep: lcurl (lua-cURLv3) -> .deps/lua/lcurl.so.
#
# Sources are vendored in-tree under watchdog/third_party/lua-curl/src (a curated
# C subset; see watchdog/third_party/lua-curl/UPSTREAM for provenance). There is
# no build-time download and no pin to verify -- the compiled bytes are exactly
# the in-tree source. libcurl must be installed on the host. JSON is pure Lua
# under watchdog/third_party/json.lua (no compile step).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
src_dir="${root}/watchdog/third_party/lua-curl/src"
out_dir="${root}/.deps/lua"
out_so="${out_dir}/lcurl.so"

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

# Rebuild only if the .so is missing/unloadable or older than any vendored source.
needs_build=1
if [[ -f "${out_so}" ]] && lcurl_loadable; then
    needs_build=0
    while IFS= read -r src; do
        if [[ "${src}" -nt "${out_so}" ]]; then
            needs_build=1
            break
        fi
    done < <(find "${src_dir}" -type f \( -name '*.c' -o -name '*.h' \))
fi
if [[ "${needs_build}" -eq 0 ]]; then
    exit 0
fi

# Lua headers: prefer pkg-config (covers nix / Homebrew / Debian), then the
# usual Debian include dirs, then an explicit LUA_INC override.
lua_cflags=""
if [[ -n "${LUA_INC:-}" ]]; then
    lua_cflags="-I${LUA_INC}"
else
    for impl in lua5.4 lua5.3 lua; do
        if pkg-config --exists "${impl}" 2>/dev/null; then
            lua_cflags="$(pkg-config --cflags "${impl}")"
            break
        fi
    done
fi
if [[ -z "${lua_cflags}" ]]; then
    for dir in /usr/include/lua5.4 /usr/include/lua5.3 /usr/include/lua; do
        if [[ -f "${dir}/lua.h" ]]; then
            lua_cflags="-I${dir}"
            break
        fi
    done
fi
if [[ -z "${lua_cflags}" ]]; then
    echo "watchdog-lua-deps: Lua headers not found; install lua5.4-dev or set LUA_INC" >&2
    exit 1
fi

if ! pkg-config --exists libcurl 2>/dev/null; then
    echo "watchdog-lua-deps: libcurl dev package not found (libcurl4-openssl-dev or similar)" >&2
    exit 1
fi

# Match the upstream Makefile's essential flags; on macOS a Lua C module is a
# bundle with dynamic_lookup, on Linux a plain shared object.
case "$(uname)" in
    Darwin) os_flags=(-bundle -undefined dynamic_lookup) ;;
    *) os_flags=(-shared) ;;
esac

echo "watchdog-lua-deps: compiling vendored lcurl.so" >&2
# shellcheck disable=SC2046  # intentional word-splitting of pkg-config output
"${CC:-cc}" -O2 -pipe -fPIC "${os_flags[@]}" -Wall -Wno-unused-value -DPTHREADS \
    ${lua_cflags} $(pkg-config --cflags libcurl) \
    "${src_dir}"/*.c \
    -o "${out_so}" \
    $(pkg-config --libs libcurl)

if [[ -z "${lua_bin}" ]]; then
    echo "watchdog-lua-deps: install lua5.4 (or set LUA_BIN) to verify lcurl.so" >&2
    exit 1
fi
if ! lcurl_loadable; then
    echo "watchdog-lua-deps: built lcurl.so but ${lua_bin} cannot load it (Lua version mismatch?)" >&2
    exit 1
fi
