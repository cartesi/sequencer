#!/usr/bin/env bash
# Provide lua-cjson for watchdog tests: use system package or compile with gcc (no make).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out_dir="${root}/.deps/lua"
out_so="${out_dir}/cjson.so"

mkdir -p "${out_dir}"

cjson_loadable() {
    # Prefer in-process cpath (works across Lua 5.x); LUA_CPATH_* env names vary by version.
    lua -e "package.cpath='${out_dir}/?.so;'..package.cpath; require('cjson')" >/dev/null 2>&1
}

if [[ -f "${out_so}" ]] && cjson_loadable; then
    exit 0
fi

# Prefer distro-packaged module (Debian/Ubuntu, etc.).
for candidate in \
    /usr/lib/x86_64-linux-gnu/lua/5.4/cjson.so \
    /usr/lib/x86_64-linux-gnu/lua/5.3/cjson.so \
    /usr/lib/lua/5.4/cjson.so \
    /usr/lib/lua/5.3/cjson.so; do
    if [[ -f "${candidate}" ]]; then
        cp "${candidate}" "${out_so}"
        exit 0
    fi
done

if cjson_loadable; then
    exit 0
fi

lua_inc=""
for dir in /usr/include/lua5.4 /usr/include/lua5.3 /usr/include/lua; do
    if [[ -f "${dir}/lua.h" ]]; then
        lua_inc="${dir}"
        break
    fi
done

if [[ -z "${lua_inc}" ]]; then
    echo "watchdog-lua-deps: install lua-cjson (e.g. apt install lua-cjson) or Lua headers (lua5.4-dev)" >&2
    exit 1
fi

if ! command -v gcc >/dev/null 2>&1; then
    echo "watchdog-lua-deps: install gcc or the lua-cjson system package" >&2
    exit 1
fi

tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT

src="${tmp}/lua-cjson"
if command -v curl >/dev/null 2>&1; then
    curl -fsSL "https://github.com/openresty/lua-cjson/archive/refs/heads/master.tar.gz" \
        | tar -xz -C "${tmp}"
elif command -v wget >/dev/null 2>&1; then
    wget -qO- "https://github.com/openresty/lua-cjson/archive/refs/heads/master.tar.gz" \
        | tar -xz -C "${tmp}"
else
    echo "watchdog-lua-deps: need curl or wget to fetch lua-cjson sources" >&2
    exit 1
fi

shopt -s nullglob
dirs=("${tmp}"/lua-cjson-*)
if [[ ${#dirs[@]} -ne 1 ]]; then
    echo "watchdog-lua-deps: unexpected lua-cjson extract layout" >&2
    exit 1
fi
src="${dirs[0]}"

cflags=(-O3 -Wall -pedantic -DNDEBUG -I"${lua_inc}" -fPIC)
gcc "${cflags[@]}" -c -o "${tmp}/lua_cjson.o" "${src}/lua_cjson.c"
gcc "${cflags[@]}" -c -o "${tmp}/strbuf.o" "${src}/strbuf.c"
gcc "${cflags[@]}" -c -o "${tmp}/fpconv.o" "${src}/fpconv.c"
gcc -shared -o "${out_so}" "${tmp}/lua_cjson.o" "${tmp}/strbuf.o" "${tmp}/fpconv.o"

if ! cjson_loadable; then
    echo "watchdog-lua-deps: built cjson.so but lua cannot load it (Lua version mismatch?)" >&2
    exit 1
fi
