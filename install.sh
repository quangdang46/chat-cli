#!/usr/bin/env bash
# chat-cli installer — curl -fsSL https://raw.githubusercontent.com/quangdang46/chat-cli/main/install.sh | bash
set -euo pipefail
umask 022

# === Config ===
BINARY_NAME="chat-cli"
OWNER="quangdang46"
REPO="chat-cli"
DEST="${DEST:-$HOME/.local/bin}"
VERSION="${VERSION:-}"
QUIET=0
EASY=0
VERIFY=0
FROM_SOURCE=0
UNINSTALL=0
MAX_RETRIES=3
DOWNLOAD_TIMEOUT=180
LOCK_DIR="/tmp/${BINARY_NAME}-install.lock.d"
TMP=""

# === Logging ===
log_info()    { [ "$QUIET" -eq 1 ] && return; echo "[${BINARY_NAME}] $*" >&2; }
log_warn()    { echo "[${BINARY_NAME}] WARN: $*" >&2; }
log_success() { [ "$QUIET" -eq 1 ] && return; echo "✓ $*" >&2; }
die()         { echo "ERROR: $*" >&2; exit 1; }

usage() {
    cat <<USAGE
${BINARY_NAME} installer

Usage: install.sh [options]

Options:
    --dest <dir>      Install directory (default: ~/.local/bin)
    --dest=<dir>      Same as --dest=
    --version <tag>   Install a specific release tag (e.g. v0.1.0)
    --system          Install to /usr/local/bin (needs write access)
    --easy-mode       Auto-add DEST to ~/.zshrc / ~/.bashrc PATH
    --verify          Run a smoke check after install
    --from-source     Build from source with cargo instead of downloading
    --quiet, -q       Suppress progress output
    --uninstall       Remove the binary and PATH lines added by easy-mode
    -h, --help        Show this help

Environment:
    DEST              Override install dir without flags
    VERSION           Override version tag without flags

Examples:
    curl -fsSL https://raw.githubusercontent.com/${OWNER}/${REPO}/main/install.sh | bash
    ./install.sh --easy-mode --verify
    ./install.sh --uninstall
USAGE
}

# === Cleanup & lock ===
cleanup() { rm -rf "$TMP" "$LOCK_DIR" 2>/dev/null || true; }
trap cleanup EXIT
acquire_lock() {
    mkdir "$LOCK_DIR" 2>/dev/null || die "Another install running. If stuck: rm -rf $LOCK_DIR"
    echo $$ > "$LOCK_DIR/pid"
}

# === Args ===
while [ $# -gt 0 ]; do
    case "$1" in
        --dest)        DEST="$2";        shift 2;;
        --dest=*)      DEST="${1#*=}";   shift;;
        --version)     VERSION="$2";     shift 2;;
        --version=*)   VERSION="${1#*=}"; shift;;
        --system)      DEST="/usr/local/bin"; shift;;
        --easy-mode)   EASY=1;           shift;;
        --verify)      VERIFY=1;         shift;;
        --from-source) FROM_SOURCE=1;    shift;;
        --quiet|-q)    QUIET=1;          shift;;
        --uninstall)   UNINSTALL=1;      shift;;
        -h|--help)     usage;            exit 0;;
        *) shift;;
    esac
done

# === Uninstall ===
if [ "$UNINSTALL" -eq 1 ]; then
    rm -f "$DEST/$BINARY_NAME"
    for rc in "$HOME/.bashrc" "$HOME/.zshrc"; do
        [ -f "$rc" ] && sed -i.bak "/${BINARY_NAME} installer/d" "$rc" 2>/dev/null || true
    done
    log_success "${BINARY_NAME} uninstalled from $DEST"
    exit 0
fi

# === Platform ===
detect_platform() {
    local os arch
    case "$(uname -s)" in
        Linux*)              os="linux";;
        Darwin*)             os="darwin";;
        MINGW*|MSYS*|CYGWIN*) os="windows";;
        *) die "Unsupported OS: $(uname -s)";;
    esac
    case "$(uname -m)" in
        x86_64|amd64)  arch="x86_64";;
        aarch64|arm64) arch="aarch64";;
        *) die "Unsupported arch: $(uname -m)";;
    esac
    echo "${os}_${arch}"
}

# Map platform to release asset suffix: darwin_aarch64 -> macos-aarch64 etc.
asset_suffix() {
    case "$1" in
        linux_x86_64)   echo "linux-x86_64";;
        linux_aarch64)  echo "linux-aarch64";;
        darwin_x86_64)  echo "macos-x86_64";;
        darwin_aarch64) echo "macos-aarch64";;
        windows_x86_64) echo "windows-x86_64";;
        *) die "No release asset for platform '$1'";;
    esac
}

# === Version ===
resolve_version() {
    [ -n "$VERSION" ] && return 0
    VERSION=$(curl -fsSL --connect-timeout 10 --max-time 30 \
        -H "Accept: application/vnd.github.v3+json" \
        "https://api.github.com/repos/${OWNER}/${REPO}/releases/latest" 2>/dev/null \
        | grep '"tag_name":' | sed -E 's/.*"v?([^"]+)".*/\1/') || true
    if ! [[ "$VERSION" =~ ^v[0-9] ]]; then
        VERSION=$(curl -fsSL -o /dev/null -w '%{url_effective}' \
            "https://github.com/${OWNER}/${REPO}/releases/latest" 2>/dev/null \
            | sed -E 's|.*/tag/||') || true
    fi
    [[ "$VERSION" =~ ^v[0-9] ]] || die "Could not resolve latest version — pass --version vX.Y.Z"
    log_info "Latest release: $VERSION"
}

# === Download ===
download_file() {
    local url="$1" dest="$2" partial="${2}.part" attempt=0
    while [ $attempt -lt $MAX_RETRIES ]; do
        attempt=$((attempt + 1))
        if curl -fL \
                --connect-timeout 30 \
                --max-time "$DOWNLOAD_TIMEOUT" \
                --retry 2 \
                $( [ -s "$partial" ] && echo "--continue-at -" ) \
                $( [ "$QUIET" -eq 0 ] && [ -t 2 ] && echo "--progress-bar" || echo "-sS" ) \
                -o "$partial" "$url"; then
            mv -f "$partial" "$dest" && return 0
        fi
        [ $attempt -lt $MAX_RETRIES ] && { log_warn "Download retry $attempt/$MAX_RETRIES..."; sleep 3; }
    done
    return 1
}

# === Atomic install ===
install_binary_atomic() {
    local src="$1" dest="$2" tmp="${2}.tmp.$$"
    install -m 0755 "$src" "$tmp" && mv -f "$tmp" "$dest" \
        || { rm -f "$tmp"; die "Failed to install binary to $dest"; }
}

# === PATH ===
maybe_add_path() {
    case ":$PATH:" in *":$DEST:"*) return 0;; esac
    if [ "$EASY" -eq 1 ]; then
        for rc in "$HOME/.zshrc" "$HOME/.bashrc"; do
            [ -f "$rc" ] && [ -w "$rc" ] || continue
            grep -qF "$DEST" "$rc" && continue
            printf '\nexport PATH="%s:$PATH"  # %s installer\n' "$DEST" "$BINARY_NAME" >> "$rc"
        done
        log_warn "PATH updated — restart shell or run: export PATH=\"$DEST:\$PATH\""
    else
        log_warn "Add to PATH: export PATH=\"$DEST:\$PATH\""
    fi
}

# === Source build ===
build_from_source() {
    command -v cargo >/dev/null || die "cargo not found — install Rust: https://rustup.rs"
    git clone --depth 1 "https://github.com/${OWNER}/${REPO}.git" "$TMP/src"
    (cd "$TMP/src" && CARGO_TARGET_DIR="$TMP/target" cargo build --release -p chat-cli)
    local bin="$TMP/target/release/$BINARY_NAME"
    [[ "$platform" == windows* ]] && bin="${bin}.exe"
    [ -f "$bin" ] || die "Source build produced no binary at $bin"
    install_binary_atomic "$bin" "$DEST/$BINARY_NAME"
}

# === Main ===
main() {
    acquire_lock
    TMP=$(mktemp -d)
    mkdir -p "$DEST"

    platform=$(detect_platform)
    suffix=$(asset_suffix "$platform")
    log_info "Platform: $platform | Dest: $DEST"

    if [ "$FROM_SOURCE" -eq 0 ]; then
        resolve_version
        ext="tar.gz"
        [[ "$platform" == windows* ]] && ext="zip"
        archive="${BINARY_NAME}-${VERSION}-${suffix}.${ext}"
        url="https://github.com/${OWNER}/${REPO}/releases/download/${VERSION}/${archive}"

        if download_file "$url" "$TMP/$archive"; then
            # Verify checksum when sidecar exists
            if download_file "${url}.sha256" "$TMP/checksum.sha256" 2>/dev/null; then
                expected=$(awk '{print $1}' "$TMP/checksum.sha256")
                actual=$(sha256sum "$TMP/$archive" 2>/dev/null | awk '{print $1}' \
                      || shasum -a 256 "$TMP/$archive" | awk '{print $1}')
                [ "$expected" = "$actual" ] || die "Checksum mismatch for $archive"
                log_success "Checksum verified"
            else
                log_warn "Checksum sidecar unavailable — skipping verification"
            fi
            case "$archive" in
                *.tar.gz) tar -xzf "$TMP/$archive" -C "$TMP";;
                *.zip)    unzip -qo "$TMP/$archive" -d "$TMP";;
            esac
            exe="$BINARY_NAME"
            [[ "$platform" == windows* ]] && exe="${BINARY_NAME}.exe"
            bin=$(find "$TMP" -name "$exe" -type f -perm -111 2>/dev/null | head -1)
            # Windows zips lose the exec bit under find -perm on some systems
            [ -n "$bin" ] || bin=$(find "$TMP" -name "$exe" -type f 2>/dev/null | head -1)
            [ -n "$bin" ] || die "Binary not found after extract"
            install_binary_atomic "$bin" "$DEST/$exe"
        else
            log_warn "Binary download failed — falling back to source build..."
            build_from_source
        fi
    else
        build_from_source
    fi

    maybe_add_path

    if [ "$VERIFY" -eq 1 ]; then
        log_info "Smoke check:"
        "$DEST/$BINARY_NAME" auth status 2>&1 | head -3 || true
    fi

    echo ""
    echo "✓ ${BINARY_NAME} installed → $DEST/$BINARY_NAME"
    echo ""
    echo "  Quick start:"
    echo "    ${BINARY_NAME} auth login deepseek --token <TOKEN>"
    echo "    ${BINARY_NAME} -p \"hi\" --provider deepseek"
    echo "    ${BINARY_NAME} --help"
    echo ""
}

# curl|bash safety: buffer entire script before executing
if [[ "${BASH_SOURCE[0]:-}" == "${0:-}" ]] || [[ -z "${BASH_SOURCE[0]:-}" ]]; then
    { main "$@"; }
fi
