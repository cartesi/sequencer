#!/usr/bin/env bash
# Build the watchdog's native Lua modules into .deps/lua/:
#   lcurl.so -- lua-cURLv3 (HTTP), from watchdog/third_party/lua-curl/src
#   lfs.so   -- LuaFileSystem (directories), from watchdog/third_party/luafilesystem/src
#
# Sources are vendored in-tree (see each directory's UPSTREAM file). There is no
# build-time download and no pin to verify -- the compiled bytes are exactly the
# in-tree source. libcurl must be installed on the host. JSON is pure Lua under
# watchdog/third_party/json.lua (no compile step).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
third_party="${root}/watchdog/third_party"
out_dir="${root}/.deps/lua"

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
if [[ -z "${lua_bin}" ]]; then
    echo "watchdog-lua-deps: install lua5.4 (or set LUA_BIN) to verify the modules" >&2
    exit 1
fi

loadable() {
    "${lua_bin}" -e "package.cpath='${out_dir}/?.so;'..package.cpath; require('$1')" >/dev/null 2>&1
}

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

# On macOS a Lua C module is a bundle with dynamic_lookup, on Linux a plain
# shared object.
case "$(uname)" in
    Darwin) os_flags=(-bundle -undefined dynamic_lookup) ;;
    *) os_flags=(-shared) ;;
esac

# build_module <module> <source dir> [extra compiler/linker args...]
# Rebuilds only if the .so is missing/unloadable or older than any source.
build_module() {
    local module="$1" src_dir="$2"
    shift 2
    local out_so="${out_dir}/${module}.so"
    if [[ -f "${out_so}" ]] && loadable "${module}" \
        && [[ -z "$(find "${src_dir}" -type f \( -name '*.c' -o -name '*.h' \) -newer "${out_so}")" ]]; then
        return 0
    fi
    if [[ -z "${lua_cflags}" ]]; then
        echo "watchdog-lua-deps: Lua headers not found; install the Lua 5.4 headers (liblua5.4-dev) or set LUA_INC" >&2
        exit 1
    fi
    echo "watchdog-lua-deps: compiling vendored ${module}.so" >&2
    # shellcheck disable=SC2086  # intentional word-splitting of the Lua cflags
    "${CC:-cc}" -O2 -pipe -fPIC "${os_flags[@]}" -Wall -Wno-unused-value \
        ${lua_cflags} "${src_dir}"/*.c -o "${out_so}" "$@"
    if ! loadable "${module}"; then
        echo "watchdog-lua-deps: built ${module}.so but ${lua_bin} cannot load it (Lua version mismatch?)" >&2
        exit 1
    fi
}

if ! pkg-config --exists libcurl 2>/dev/null; then
    echo "watchdog-lua-deps: libcurl dev package not found (libcurl4-openssl-dev or similar)" >&2
    exit 1
fi
# shellcheck disable=SC2046  # intentional word-splitting of pkg-config output
build_module lcurl "${third_party}/lua-curl/src" -DPTHREADS \
    $(pkg-config --cflags libcurl) $(pkg-config --libs libcurl)
build_module lfs "${third_party}/luafilesystem/src"
