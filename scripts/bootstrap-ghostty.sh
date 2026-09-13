#!/usr/bin/env sh
set -eu

GHOSTTY_COMMIT=7aab0a0392369613472bd5dcfd66bef58e78c3ec
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PROJECT_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
TOOLS_DIR="$PROJECT_DIR/target/forge-tools"
SOURCE_DIR="$TOOLS_DIR/ghostty-$GHOSTTY_COMMIT"
PREFIX_DIR="$PROJECT_DIR/target/ghostty"
ARCHIVE="$TOOLS_DIR/ghostty-$GHOSTTY_COMMIT.tar.gz"

if [ -n "${FORGE_ZIG:-}" ]; then
    ZIG_BIN=$FORGE_ZIG
else
    ZIG_BIN=$(command -v zig || true)
fi

if [ -z "$ZIG_BIN" ]; then
    echo "Zig 0.16.0 is required. Set FORGE_ZIG=/path/to/zig." >&2
    exit 2
fi

if [ "$($ZIG_BIN version)" != "0.16.0" ]; then
    echo "Expected Zig 0.16.0, found $($ZIG_BIN version)." >&2
    exit 2
fi
ZIG_BIN=$(command -v "$ZIG_BIN")

STAMP="$PREFIX_DIR/.ghostty-commit"

# A restored CI cache may contain the built library, or a half-pruned source
# tree. Trust only a build stamped with this exact commit, and re-extract
# whenever build.zig is missing.
if [ -f "$STAMP" ] && [ "$(cat "$STAMP")" = "$GHOSTTY_COMMIT" ]; then
    case "$(uname -s)" in
        Darwin) EXISTING="$PREFIX_DIR/lib/libghostty-vt.dylib" ;;
        Linux) EXISTING="$PREFIX_DIR/lib/libghostty-vt.so" ;;
        *) EXISTING="$PREFIX_DIR/bin/ghostty-vt.dll" ;;
    esac
    if [ -f "$EXISTING" ]; then
        echo "$EXISTING"
        exit 0
    fi
fi

mkdir -p "$TOOLS_DIR"
if [ ! -f "$SOURCE_DIR/build.zig" ]; then
    rm -rf "$SOURCE_DIR"
    if [ ! -f "$ARCHIVE" ] || ! tar -tzf "$ARCHIVE" >/dev/null 2>&1; then
        curl -fL \
            "https://github.com/ghostty-org/ghostty/archive/$GHOSTTY_COMMIT.tar.gz" \
            -o "$ARCHIVE"
    fi
    tar -xzf "$ARCHIVE" -C "$TOOLS_DIR"
fi

(
    cd "$SOURCE_DIR"
    ZIG_GLOBAL_CACHE_DIR="$PROJECT_DIR/target/zig-global-cache" \
        "$ZIG_BIN" build \
        -Demit-lib-vt=true \
        -Dapp-runtime=none \
        -Doptimize=ReleaseFast \
        --prefix "$PREFIX_DIR"
)

case "$(uname -s)" in
    Darwin) LIBRARY="$PREFIX_DIR/lib/libghostty-vt.dylib" ;;
    Linux)
        VERSIONED_LIBRARY="$PREFIX_DIR/lib/libghostty-vt.so.0.1.0"
        LIBRARY="$PREFIX_DIR/lib/libghostty-vt.so"
        if [ -f "$VERSIONED_LIBRARY" ]; then
            ln -sf "$(basename "$VERSIONED_LIBRARY")" "$LIBRARY"
        fi
        ;;
    *) LIBRARY="$PREFIX_DIR/bin/ghostty-vt.dll" ;;
esac

if [ ! -f "$LIBRARY" ]; then
    echo "Ghostty build completed but $LIBRARY was not produced." >&2
    exit 1
fi

echo "$GHOSTTY_COMMIT" > "$STAMP"
echo "$LIBRARY"
