#!/usr/bin/env bash
# ro installer (Linux + macOS, x86_64 + aarch64).
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/quangdang46/repo_orchestrator/main/install.sh | bash
#
# Environment overrides:
#   RO_VERSION       Tag to install (e.g. v0.1.0). Default: latest GitHub release.
#   RO_INSTALL_DIR   Where the `ro` binary is placed. Default: $HOME/.local/bin.
#   RO_NO_VERIFY     If set to 1, skip SHA256 verification (NOT recommended).
#   RO_FORCE         If set to 1, overwrite an existing binary without prompting.
#
# Exit codes:
#   0  success
#   1  generic failure
#   2  unsupported platform
#   3  network / download failure
#   4  checksum mismatch

set -euo pipefail

REPO="quangdang46/repo_orchestrator"
BIN="ro"
VERSION="${RO_VERSION:-latest}"
INSTALL_DIR="${RO_INSTALL_DIR:-$HOME/.local/bin}"
NO_VERIFY="${RO_NO_VERIFY:-0}"
FORCE="${RO_FORCE:-0}"

# ---------- pretty output ----------
if [ -t 1 ] && command -v tput >/dev/null 2>&1 && [ "$(tput colors 2>/dev/null || echo 0)" -ge 8 ]; then
    C_RESET="$(tput sgr0)"
    C_BOLD="$(tput bold)"
    C_RED="$(tput setaf 1)"
    C_GREEN="$(tput setaf 2)"
    C_YELLOW="$(tput setaf 3)"
    C_BLUE="$(tput setaf 4)"
else
    C_RESET="" ; C_BOLD="" ; C_RED="" ; C_GREEN="" ; C_YELLOW="" ; C_BLUE=""
fi

info()  { printf "%s==>%s %s\n"        "$C_BLUE"   "$C_RESET" "$*" >&2; }
ok()    { printf "%s ✓ %s%s\n"         "$C_GREEN"  "$*"       "$C_RESET" >&2; }
warn()  { printf "%s ! %s%s\n"         "$C_YELLOW" "$*"       "$C_RESET" >&2; }
die()   { printf "%s x %s%s\n"         "$C_RED"   "$*"       "$C_RESET" >&2; exit 1; }
err()   { printf "%s ✗ %s%s\n"         "$C_RED"    "$*"       "$C_RESET" >&2; }

# ---------- helpers ----------
need() {
    if ! command -v "$1" >/dev/null 2>&1; then
        err "required command not found: $1"
        exit 1
    fi
}

cleanup() {
    if [ -n "${TMPDIR_RO:-}" ] && [ -d "$TMPDIR_RO" ]; then
        rm -rf "$TMPDIR_RO"
    fi
}
trap cleanup EXIT INT TERM

http_get() {
    # http_get <url> <out>
    if command -v curl >/dev/null 2>&1; then
        curl --proto '=https' --tlsv1.2 -fsSL --retry 3 --retry-delay 2 -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget --https-only --tries=3 -qO "$2" "$1"
    else
        err "neither curl nor wget is installed"
        exit 1
    fi
}

http_get_stdout() {
    if command -v curl >/dev/null 2>&1; then
        curl --proto '=https' --tlsv1.2 -fsSL --retry 3 --retry-delay 2 "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget --https-only --tries=3 -qO- "$1"
    else
        err "neither curl nor wget is installed"
        exit 1
    fi
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        err "no SHA256 tool found (need sha256sum or shasum)"
        exit 1
    fi
}

# ---------- detection ----------
detect_target() {
    local os arch
    os="$(uname -s 2>/dev/null || echo unknown)"
    arch="$(uname -m 2>/dev/null || echo unknown)"

    case "$os" in
        Linux)  os="linux"  ;;
        Darwin) os="darwin" ;;
        *)
            err "unsupported OS: $os (this script supports Linux and macOS; use install.ps1 on Windows)"
            exit 2
            ;;
    esac

    case "$arch" in
        x86_64|amd64)        arch="x86_64"  ;;
        aarch64|arm64)       arch="aarch64" ;;
        *)
            err "unsupported architecture: $arch (supported: x86_64, aarch64)"
            exit 2
            ;;
    esac

    case "${os}-${arch}" in
        linux-x86_64)   echo "x86_64-unknown-linux-musl"  ;;
        linux-aarch64)  echo "aarch64-unknown-linux-musl" ;;
        darwin-x86_64)  echo "x86_64-apple-darwin"        ;;
        darwin-aarch64) echo "aarch64-apple-darwin"       ;;
        *)
            err "unsupported platform: ${os}-${arch}"
            exit 2
            ;;
    esac
}

# ---------- version resolution ----------
resolve_version() {
    if [ "$VERSION" = "latest" ]; then
        local api="https://api.github.com/repos/${REPO}/releases/latest"
        local tag
        tag="$(http_get_stdout "$api" \
            | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
            | head -n1)"
        if [ -z "$tag" ]; then
            err "could not resolve latest release tag from $api"
            err "GitHub may be rate-limiting; pin a version with RO_VERSION=v0.1.0"
            exit 3
        fi

        # Verify the release has assets; a tag-only release (CI still
        # running or failed) will 404 on the actual artifact URL.
        local assets
        assets="$(http_get_stdout "$api" \
            | sed -n 's/.*"assets"[[:space:]]*:[[:space:]]*\[\]/EMPTY/p' \
            | head -n1)"
        if [ "$assets" = "EMPTY" ]; then
            err "release ${tag} has no assets yet (CI may still be building)"
            err "wait a few minutes and retry, or build from source:"
            err "  git clone https://github.com/${REPO} && cd repo_orchestrator && cargo build --release"
            exit 3
        fi

        printf '%s' "$tag"
    else
        case "$VERSION" in
            v*) printf '%s' "$VERSION" ;;
            *)  printf 'v%s' "$VERSION" ;;
        esac
    fi
}

# ---------- main ----------
main() {
    need uname
    need tar

    info "ro installer"
    info "repo:   https://github.com/${REPO}"
    info "user:   $(id -un 2>/dev/null || echo unknown)"

    local target tag archive_name archive_url checksum_url
    target="$(detect_target)"
    info "target: ${C_BOLD}${target}${C_RESET}"

    tag="$(resolve_version)"
    info "version: ${C_BOLD}${tag}${C_RESET}"

    archive_name="${BIN}-${target}.tar.xz"
    archive_url="https://github.com/${REPO}/releases/download/${tag}/${archive_name}"
    checksum_url="${archive_url}.sha256"

    TMPDIR_RO="$(mktemp -d 2>/dev/null || mktemp -d -t ro-install)"

    info "downloading ${archive_name}"
    if ! http_get "$archive_url" "${TMPDIR_RO}/${archive_name}"; then
        err "failed to download $archive_url"
        err "check that release ${tag} exists and includes ${archive_name}"
        exit 3
    fi
    ok "downloaded $(du -h "${TMPDIR_RO}/${archive_name}" | awk '{print $1}')"

    if [ "$NO_VERIFY" != "1" ]; then
        info "verifying SHA256"
        if ! http_get "$checksum_url" "${TMPDIR_RO}/${archive_name}.sha256"; then
            err "failed to download checksum from $checksum_url"
            err "set RO_NO_VERIFY=1 to skip (not recommended)"
            exit 3
        fi
        local expected actual
        expected="$(awk '{print $1}' "${TMPDIR_RO}/${archive_name}.sha256")"
        actual="$(sha256_of "${TMPDIR_RO}/${archive_name}")"
        if [ "$expected" != "$actual" ]; then
            err "SHA256 mismatch!"
            err "  expected: $expected"
            err "  actual:   $actual"
            exit 4
        fi
        ok "SHA256 verified"
    else
        warn "RO_NO_VERIFY=1 set; skipping checksum"
    fi

    info "extracting archive"
    ( cd "$TMPDIR_RO" && tar -xf "$archive_name" )

    # cargo-dist lays out the archive as: <bin>-<target>/<bin>
    local extracted="${TMPDIR_RO}/${BIN}-${target}/${BIN}"
    if [ ! -f "$extracted" ]; then
        # Fall back to a recursive find in case the layout changes.
        extracted="$(find "$TMPDIR_RO" -type f -name "$BIN" -perm -u+x 2>/dev/null | head -n1 || true)"
    fi
    if [ -z "$extracted" ] || [ ! -f "$extracted" ]; then
        err "could not locate '${BIN}' binary inside ${archive_name}"
        exit 1
    fi

    mkdir -p "$INSTALL_DIR"
    local dest="${INSTALL_DIR%/}/${BIN}"
    # `RO_FORCE=0` is the default, and it means *refuse*. The old code
    # warned about it while installing unconditionally, so the warning
    # named the value the user already had and promised a refusal that
    # never happened. A safeguard that does not fire is worse than none.
    if [ -e "$dest" ] && [ "$FORCE" != "1" ]; then
        die "$dest already exists. Re-run with RO_FORCE=1 to replace it."
    fi

    install -m 0755 "$extracted" "$dest" 2>/dev/null || {
        cp "$extracted" "$dest"
        chmod 0755 "$dest"
    }
    ok "installed: $dest"

    # Sanity check
    if "$dest" --version >/dev/null 2>&1; then
        local ver
        ver="$("$dest" --version 2>/dev/null | head -n1)"
        ok "${ver}"
    else
        warn "installed binary did not respond to --version (may still work)"
    fi

    # PATH hint
    case ":${PATH:-}:" in
        *":${INSTALL_DIR%/}:"*) : ;;
        *)
            warn "${INSTALL_DIR%/} is not in your PATH."
            cat >&2 <<EOF

  Add it by appending one of these to your shell profile, then restart your shell:

    # bash / zsh
    echo 'export PATH="${INSTALL_DIR%/}:\$PATH"' >> ~/.bashrc
    echo 'export PATH="${INSTALL_DIR%/}:\$PATH"' >> ~/.zshrc

    # fish
    fish_add_path "${INSTALL_DIR%/}"

EOF
            ;;
    esac

    ok "done. run: ${C_BOLD}${BIN} --help${C_RESET}"
}

main "$@"
