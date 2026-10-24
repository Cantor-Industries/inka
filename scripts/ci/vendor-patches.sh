#!/usr/bin/env bash
# Verify vendored code against its recorded upstream base + patch series.
#
# Each unit lives in patches/<unit>/ with metadata.env, series and patch files.
# The exact upstream source is fetched (or supplied with --upstream), its base
# sha256 is checked, the series is applied, and the result is diffed against the
# committed local copy. Any drift fails the script.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PATCHES="$ROOT/patches"
ONLY=""
UPSTREAM_OVERRIDE=""
LIST=0
WORK=""

usage() {
    cat <<'EOF'
usage: scripts/ci/vendor-patches.sh [--list] [--only UNIT] [--upstream PATH]

  --list             list units and exit
  --only UNIT        verify one unit
  --upstream PATH    offline base: upstream file for `file` units, or an
                     extracted crate root for `crate` units (requires --only)
EOF
}

cleanup() {
    if [ -n "$WORK" ]; then
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT

while [ "$#" -gt 0 ]; do
    case "$1" in
        --list) LIST=1 ;;
        --only)
            [ "$#" -ge 2 ] || { echo "error: --only needs a unit" >&2; exit 2; }
            ONLY="$2"; shift ;;
        --upstream)
            [ "$#" -ge 2 ] || { echo "error: --upstream needs a path" >&2; exit 2; }
            UPSTREAM_OVERRIDE="$2"; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "error: unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        echo "error: no sha256 tool (sha256sum or shasum)" >&2
        exit 2
    fi
}

find_units() {
    find "$PATCHES" -mindepth 2 -maxdepth 2 -name metadata.env -print \
        | sort \
        | sed 's#/metadata.env$##'
}

if [ "$LIST" -eq 1 ]; then
    while IFS= read -r unit_dir; do
        # shellcheck disable=SC1090
        source "$unit_dir/metadata.env"
        printf '%-20s %-6s %s\n' "${UNIT:-$(basename "$unit_dir")}" "${KIND:-?}" "${LOCAL_PATH:-?}"
    done < <(find_units)
    exit 0
fi

units=()
while IFS= read -r unit_dir; do
    units+=("$unit_dir")
done < <(find_units)

[ "${#units[@]}" -gt 0 ] || { echo "error: no patch units under $PATCHES" >&2; exit 1; }

if [ -n "$ONLY" ] && [ -z "$UPSTREAM_OVERRIDE" ]; then
    :
elif [ -n "$UPSTREAM_OVERRIDE" ] && [ -z "$ONLY" ]; then
    echo "error: --upstream requires --only" >&2
    exit 2
fi

matched=0
for unit_dir in "${units[@]}"; do
    # shellcheck disable=SC1090
    source "$unit_dir/metadata.env"
    name="${UNIT:-$(basename "$unit_dir")}"
    if [ -n "$ONLY" ] && [ "$name" != "$ONLY" ]; then
        continue
    fi
    matched=$((matched + 1))

    : "${KIND:?metadata missing KIND in $unit_dir}"
    : "${UPSTREAM_URL:?metadata missing UPSTREAM_URL in $unit_dir}"
    : "${BASE_SHA256:?metadata missing BASE_SHA256 in $unit_dir}"
    : "${LOCAL_PATH:?metadata missing LOCAL_PATH in $unit_dir}"

    WORK="$(mktemp -d "${TMPDIR:-/tmp}/inka-vendor-${name}.XXXXXX")"
    src=""

    case "$KIND" in
        crate)
            : "${CRATE:?metadata missing CRATE in $unit_dir}"
            : "${VERSION:?metadata missing VERSION in $unit_dir}"
            src="$WORK/$CRATE-$VERSION"
            if [ -n "$UPSTREAM_OVERRIDE" ]; then
                mkdir -p "$src"
                cp -a "$UPSTREAM_OVERRIDE/." "$src/"
            else
                crate="$WORK/upstream.crate"
                curl -fsSL "$UPSTREAM_URL" -o "$crate"
                actual="$(sha256_file "$crate")"
                if [ "$actual" != "$BASE_SHA256" ]; then
                    echo "error: $name base sha256 mismatch" >&2
                    echo "  expected $BASE_SHA256" >&2
                    echo "  actual   $actual" >&2
                    exit 1
                fi
                tar -xzf "$crate" -C "$WORK"
            fi
            # The published crate tarball carries packaging metadata that is not
            # part of the source tree; our vendor copy omits it.
            rm -f "$src/Cargo.toml.orig" "$src/Cargo.lock" "$src/.cargo-ok" \
                  "$src/.cargo_vcs_info.json"
            patch_root="$src"
            ;;
        file)
            src="$WORK/$LOCAL_PATH"
            mkdir -p "$(dirname "$src")"
            if [ -n "$UPSTREAM_OVERRIDE" ]; then
                cp "$UPSTREAM_OVERRIDE" "$src"
            else
                curl -fsSL "$UPSTREAM_URL" -o "$src"
            fi
            actual="$(sha256_file "$src")"
            if [ "$actual" != "$BASE_SHA256" ]; then
                echo "error: $name base sha256 mismatch" >&2
                echo "  expected $BASE_SHA256" >&2
                echo "  actual   $actual" >&2
                exit 1
            fi
            patch_root="$WORK"
            ;;
        *)
            echo "error: $name has unknown KIND=$KIND" >&2
            exit 1
            ;;
    esac

    series="$unit_dir/series"
    if [ -f "$series" ]; then
        while IFS= read -r patch_file; do
            patch_file="${patch_file%%#*}"
            patch_file="$(printf '%s' "$patch_file" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')"
            [ -z "$patch_file" ] && continue
            if ! patch -s -p1 -d "$patch_root" < "$unit_dir/$patch_file"; then
                echo "error: $name failed to apply $patch_file" >&2
                exit 1
            fi
        done < "$series"
    fi

    if [ "$KIND" = "crate" ]; then
        diff_args=(-ru)
        if [ -n "${EXCLUDE:-}" ]; then
            IFS=',' read -r -a excludes <<< "$EXCLUDE"
            for ex in "${excludes[@]}"; do
                [ -n "$ex" ] && diff_args+=("--exclude=$ex")
            done
        fi
        # The vendor note and packaging metadata are local-only.
        diff_args+=("--exclude=README.inka.md")
        if ! diff "${diff_args[@]}" "$src" "$ROOT/$LOCAL_PATH"; then
            echo "error: $name vendored tree differs from upstream + series" >&2
            exit 1
        fi
    else
        if ! cmp -s "$src" "$ROOT/$LOCAL_PATH"; then
            echo "error: $name vendored file differs from upstream + series" >&2
            diff -u "$src" "$ROOT/$LOCAL_PATH" || true
            exit 1
        fi
    fi

    echo "OK $name"
    rm -rf "$WORK"
    WORK=""
done

if [ -n "$ONLY" ] && [ "$matched" -eq 0 ]; then
    echo "error: no unit named '$ONLY'" >&2
    exit 1
fi
