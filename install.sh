#!/bin/sh
# inka bootstrap installer.
#
# Downloads the inka toolchain (CLI + launcher),
# installs it under <prefix>/lib/inka with a symlink in <prefix>/bin, then
# provisions the shared runtime via `inka update`.
# Like rustup, it installs per-user (no root). A pre-0.5.0 install is reset
# first (0.5.0 is a clean break: the package store and vendoring were removed
# and the runtime tuple moved), then the runtime is provisioned fresh. Later
# 0.5.x installs are ordinary upgrades.
#
# usage:
#   curl --proto '=https' --tlsv1.2 -fsSL \
#     https://github.com/Cantor-Industries/inka/releases/latest/download/install.sh | sh
#
# options:
#   -y, --yes              non-interactive (accepted for compatibility)
#       --version <tag>    install a specific release tag (default: latest)
#       --from <dir-or-url> release base override (mirrors, local staging)
#       --prefix <dir>     toolchain prefix (default: $HOME/.local)
#       --no-modify-path   do not edit shell rc files
#       --no-engine        skip the runtime
#       --no-runtime       skip the runtime .so
#       --force            reinstall the toolchain even if current
#       --uninstall        remove the toolchain (and engine) and exit
#   -h, --help             show this help
set -eu

REPO="${INKA_REPO:-Cantor-Industries/inka}"
DEFAULT_BASE="https://github.com/$REPO/releases/latest/download"

info() { printf '[inka] %s\n' "$*" >&2; }
warn() { printf '[inka] warning: %s\n' "$*" >&2; }
die() { printf '[inka] error: %s\n' "$*" >&2; exit 1; }

have() { command -v "$1" >/dev/null 2>&1; }

usage() {
    cat <<'EOF'
inka installer

usage: install.sh [options]

options:
  -y, --yes               non-interactive (accepted for compatibility)
      --version <tag>     install a specific release tag (default: latest)
      --from <dir-or-url> release base override (mirrors, local staging)
      --prefix <dir>      toolchain prefix (default: $HOME/.local)
      --no-modify-path    do not edit shell rc files
      --no-engine         skip the runtime
      --no-runtime        skip the runtime .so
      --force             reinstall the toolchain even if current
      --uninstall         remove the toolchain (and engine) and exit
  -h, --help              show this help
EOF
}

# Fetch <base>/<file> into <dest>; <base> may be an http(s) URL or a local dir.
fetch() {
    _base=$1; _file=$2; _dest=$3
    case "$_base" in
        http://*|https://*)
            if have curl; then
                if [ "${_base#https://}" != "$_base" ]; then
                    curl --proto '=https' --tlsv1.2 -fsSL "$_base/$_file" -o "$_dest"
                else
                    curl -fsSL "$_base/$_file" -o "$_dest"
                fi
            elif have wget; then
                if [ "${_base#https://}" != "$_base" ]; then
                    wget --https-only -qO "$_dest" "$_base/$_file"
                else
                    wget -qO "$_dest" "$_base/$_file"
                fi
            else
                die "need curl or wget to download over HTTP"
            fi
            ;;
        *) cp "$_base/$_file" "$_dest" ;;
    esac
}

# Print <base>/<file> to stdout.
fetch_text() {
    _base=$1; _file=$2
    case "$_base" in
        http://*|https://*)
            if have curl; then
                if [ "${_base#https://}" != "$_base" ]; then
                    curl --proto '=https' --tlsv1.2 -fsSL "$_base/$_file"
                else
                    curl -fsSL "$_base/$_file"
                fi
            elif have wget; then
                if [ "${_base#https://}" != "$_base" ]; then
                    wget --https-only -qO- "$_base/$_file"
                else
                    wget -qO- "$_base/$_file"
                fi
            else
                die "need curl or wget to download over HTTP"
            fi
            ;;
        *) cat "$_base/$_file" ;;
    esac
}

sha256_of() {
    if have sha256sum; then
        sha256sum "$1" | awk '{print $1}'
    elif have shasum; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        die "no sha256sum or shasum available"
    fi
}

verify_sha() {
    _file=$1; _sidecar=$2
    _expected=$(awk '{print $1}' "$_sidecar" | head -1 | tr 'A-F' 'a-f')
    _actual=$(sha256_of "$_file")
    if [ "$_expected" != "$_actual" ]; then
        die "checksum mismatch for $(basename "$_file"): expected $_expected, got $_actual"
    fi
}

# Read the first string value for a JSON key (works for the flat/pretty
# versions.json the release pipeline writes).
json_str() {
    printf '%s\n' "$1" | sed -n "s/.*\"$2\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" | head -1
}

add_path_block() {
    _block="# >>> inka initialize >>>
export PATH=\"$PREFIX/bin:\$PATH\"
# <<< inka initialize <<<"
    _wrote=0
    for _rc in "$HOME/.profile" "$HOME/.bashrc" "$HOME/.zshrc"; do
        [ -e "$_rc" ] || continue
        if ! grep -q '>>> inka initialize >>>' "$_rc" 2>/dev/null; then
            printf '\n%s\n' "$_block" >> "$_rc"
            info "added $PREFIX/bin to PATH in $_rc"
            _wrote=1
        fi
    done
    if [ "$_wrote" = 0 ] && [ ! -e "$HOME/.profile" ]; then
        printf '%s\n' "$_block" > "$HOME/.profile"
        info "added $PREFIX/bin to PATH in $HOME/.profile"
    fi
}

remove_path_block() {
    for _rc in "$HOME/.profile" "$HOME/.bashrc" "$HOME/.zshrc"; do
        [ -e "$_rc" ] || continue
        if grep -q '>>> inka initialize >>>' "$_rc" 2>/dev/null; then
            _tmp="$_rc.inka.$$"
            sed '/# >>> inka initialize >>>/,/# <<< inka initialize <<</d' "$_rc" > "$_tmp"
            mv "$_tmp" "$_rc"
            info "removed the inka PATH block from $_rc"
        fi
    done
}

# ---- args -------------------------------------------------------------------
YES=0
VERSION=""
FROM=""
PREFIX="${INKA_PREFIX:-$HOME/.local}"
MODIFY_PATH=1
NO_RUNTIME=0
FORCE=0
UNINSTALL=0

while [ $# -gt 0 ]; do
    case "$1" in
        -y|--yes) YES=1 ;;
        --version) VERSION="${2:?--version needs a tag}"; shift ;;
        --from) FROM="${2:?--from needs a value}"; shift ;;
        --prefix) PREFIX="${2:?--prefix needs a dir}"; shift ;;
        --no-modify-path) MODIFY_PATH=0 ;;
        --no-engine) NO_RUNTIME=1 ;;
        --no-runtime) NO_RUNTIME=1 ;;
        --force) FORCE=1 ;;
        --uninstall) UNINSTALL=1 ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown option '$1' (try --help)" ;;
    esac
    shift
done

# ---- platform ---------------------------------------------------------------
_os=$(uname -s 2>/dev/null || echo unknown)
_arch=$(uname -m 2>/dev/null || echo unknown)
case "$_os/$_arch" in
    Linux/x86_64) TARGET=x86_64-unknown-linux-gnu ;;
    *)
        die "unsupported platform '$_os/$_arch' (only Linux/x86_64 is published; on Windows use WSL2)"
        ;;
esac

# ---- uninstall --------------------------------------------------------------
if [ "$UNINSTALL" = 1 ]; then
    remove_path_block
    rm -rf "$PREFIX/lib/inka"
    rm -f "$PREFIX/bin/inka"
    rm -rf "${XDG_DATA_HOME:-$HOME/.local/share}/inka"
    info "uninstalled inka from $PREFIX"
    exit 0
fi

# ---- resolve base -----------------------------------------------------------
if [ -n "$FROM" ]; then
    BASE="$FROM"
elif [ -n "$VERSION" ]; then
    case "$VERSION" in
        v*) _ver="${VERSION#v}" ;;
        *) _ver="$VERSION" ;;
    esac
    case "$_ver" in
        ""|*[!0-9A-Za-z.+~-]*) die "invalid --version '$VERSION'" ;;
    esac
    VERSION="v$_ver"
    BASE="https://github.com/$REPO/releases/download/$VERSION"
elif [ -n "${INKA_RELEASE_BASE:-}" ]; then
    BASE="$INKA_RELEASE_BASE"
else
    BASE="$DEFAULT_BASE"
fi

TMP=$(mktemp -d "${TMPDIR:-/tmp}/inka-install.XXXXXX")
trap 'rm -rf "$TMP"' EXIT INT TERM

# ---- read versions.json -----------------------------------------------------
fetch_text "$BASE" versions.json > "$TMP/versions.json" \
    || die "cannot read versions.json from $BASE"
VERSIONS=$(cat "$TMP/versions.json")

TC_VER=$(json_str "$VERSIONS" version)
TC_ARCHIVE=$(json_str "$VERSIONS" archive)
REL=$(json_str "$VERSIONS" release)
[ -n "$REL" ] || REL="${VERSION#v}"

if [ -z "$TC_ARCHIVE" ]; then
    [ -n "$REL" ] || die "versions.json has no toolchain archive and no --version was given"
    TC_ARCHIVE="inka-toolchain-$REL-$TARGET.tar.gz"
fi
[ -n "$TC_VER" ] || TC_VER="$REL"

# The archive name comes from (possibly untrusted) release metadata; require a
# plain basename so `$TMP/$TC_ARCHIVE` cannot escape the staging dir.
case "$TC_ARCHIVE" in
    */*|*\\*|..) die "versions.json names an invalid toolchain archive '$TC_ARCHIVE'" ;;
esac

# ---- install toolchain ------------------------------------------------------
CURRENT=""
[ -f "$PREFIX/lib/inka/VERSION" ] && CURRENT=$(cat "$PREFIX/lib/inka/VERSION" 2>/dev/null || true)

# ---- previous-generation reset ----------------------------------------------
# 0.4.0 is a clean break: the resolver was retired and the runtime tuple moved.
# Detect a pre-0.4.0 install (toolchain VERSION not 0.4.x, or the retired
# resolver .so present) and remove the old toolchain + engine so the new
# release installs fresh. Fresh machines and later 0.4.x upgrades skip this.
engine_dir="${INKA_RUNTIME_HOME:-${XDG_DATA_HOME:-$HOME/.local/share}/inka/runtime}"
reset=0
case "$CURRENT" in
    ""|0.5.*) ;;
    *) reset=1 ;;
esac
if [ "$reset" = 1 ]; then
    info "resetting previous inka install (pre-0.5.0)"
    rm -rf "$PREFIX/lib/inka"
    CURRENT=""
    if [ "$NO_RUNTIME" = 0 ]; then
        rm -f "$engine_dir"/libinka_runtime-*.so
    fi
fi

if [ "$FORCE" != 1 ] && [ "$CURRENT" = "$TC_VER" ] && [ -x "$PREFIX/lib/inka/inka" ]; then
    info "inka toolchain $TC_VER is current"
else
    info "installing inka toolchain $TC_VER"
    fetch "$BASE" "$TC_ARCHIVE" "$TMP/$TC_ARCHIVE"
    fetch "$BASE" "$TC_ARCHIVE.sha256" "$TMP/$TC_ARCHIVE.sha256"
    verify_sha "$TMP/$TC_ARCHIVE" "$TMP/$TC_ARCHIVE.sha256"
    mkdir -p "$PREFIX/lib/inka"
    tar -xzf "$TMP/$TC_ARCHIVE" --no-same-owner --no-same-permissions \
        --no-absolute-filenames -C "$PREFIX/lib/inka"
    chmod 0755 "$PREFIX/lib/inka/inka" "$PREFIX/lib/inka/inka-launcher"
    printf '%s\n' "$TC_VER" > "$PREFIX/lib/inka/VERSION"
fi

mkdir -p "$PREFIX/bin"
ln -sf ../lib/inka/inka "$PREFIX/bin/inka"

if [ "$MODIFY_PATH" = 1 ]; then
    case ":$PATH:" in
        *":$PREFIX/bin:"*) ;;
        *) add_path_block ;;
    esac
fi

# ---- provision engine -------------------------------------------------------
if [ "$NO_RUNTIME" = 0 ]; then
    info "installing runtime from $BASE"
    "$PREFIX/lib/inka/inka" update --from "$BASE" --no-toolchain
fi

info "inka $TC_VER installed at $PREFIX/bin/inka"
case ":$PATH:" in
    *":$PREFIX/bin:"*) on_path=1 ;;
    *) on_path=0 ;;
esac
if [ "$on_path" = 0 ]; then
    if [ "$MODIFY_PATH" = 1 ]; then
        info "restart your shell, or run: . \"$HOME/.profile\""
    else
        info "add $PREFIX/bin to your PATH to use inka"
    fi
fi
