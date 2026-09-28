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
#   RO_BASE_URL      Base URL for release artifacts (test seam). Default: the
#                    GitHub release download URL for $REPO at the resolved tag.
#                    Set to a local server (e.g. http://127.0.0.1:8000) to test
#                    installs without hitting GitHub. Must start with http://
#                    or https://; anything else fails loudly with exit 3.
#   GITHUB_TOKEN     Optional. Only used to raise the GitHub API rate limit
#                    while resolving "latest"; the default path needs no
#                    token, and public read access is enough.
#
# Archive layout (expected inside the tarball):
#   <binary>-<target>/<binary>   e.g. ro-x86_64-unknown-linux-musl/ro
#   The checksum asset is <archive>.sha256 next to the archive on the server.
#
# Exit codes:
#   0  success
#   1  generic failure
#   2  unsupported platform
#   3  network / download failure, *or* an invalid RO_BASE_URL. A mistyped
#      RO_BASE_URL is a configuration error, not a network failure; exit 3
#      names the variable and prints what was received so the user can see
#      the typo rather than go looking at their network.
#   4  checksum mismatch
#
# RO_BASE_URL and the http:// transport:
#   A `http://` RO_BASE_URL is accepted, and the `https`-only transport
#   restriction is dropped for it — this exists to serve a locally built
#   release from a local server for testing. **http:// is therefore not a
#   general "download over plaintext" escape hatch.** Only a loopback host
#   (127.0.0.1, localhost, ::1) is accepted on `http://`; a non-loopback
#   `http://` host is refused on the same exit-3 path, and the refusal says
#   that https is required. The checksum is the only protection against a
#   malicious host for a loopback base, which is the threat model this seam
#   exists for.

set -euo pipefail

REPO="quangdang46/repo_orchestrator"
BIN="ro"
VERSION="${RO_VERSION:-latest}"
INSTALL_DIR="${RO_INSTALL_DIR:-$HOME/.local/bin}"
NO_VERIFY="${RO_NO_VERIFY:-0}"
FORCE="${RO_FORCE:-0}"
# Unset means "the GitHub release download URL for $REPO at the resolved
# tag", i.e. exactly the string this script built before the seam existed.
# Set means "the bytes live here instead" — the same directory must then hold
# both the archive and its .sha256, or the checksum 404s and the install dies
# on a verification it could not perform.
BASE_URL="${RO_BASE_URL:-}"

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

# An unset RO_BASE_URL means "build the github.com URL as always" and there
# is nothing to check. A *set* one is hand-typed by whoever runs the install,
# so it is checked here — before any download — because the alternative is
# discovering the typo inside curl's own error text ("Protocol http not
# supported or disabled in libcurl", or a bare "URL rejected: Malformed input
# to a URL function") several steps later, which reads as a broken network
# and sends people looking in the wrong place.
validate_base_url() {
    [ -n "$BASE_URL" ] || return 0
    case "$BASE_URL" in
        https://*) ;;
        http://127.0.0.1*|http://localhost*|http://\[::1\]*)
            # A locally served base, for testing. Accepts both `127.0.0.1`
            # and `localhost` (they are the same machine by construction),
            # and `[::1]` (the IPv6 loopback; the brackets are the URL form).
            ;;
        http://*)
            err "RO_BASE_URL over http:// is only for loopback (testing):"
            err "  got: ${BASE_URL}"
            err "Use https:// for anything off this machine — a checksum"
            err "confirms the bytes, not the sender, so plaintext is a real"
            err "downgrade, not a redundant precaution."
            exit 3
            ;;
        *)
            err "RO_BASE_URL must start with http:// or https://"
            err "  got: ${BASE_URL}"
            exit 3
            ;;
    esac
}

cleanup() {
    if [ -n "${TMPDIR_RO:-}" ] && [ -d "$TMPDIR_RO" ]; then
        rm -rf "$TMPDIR_RO"
    fi
}
trap cleanup EXIT INT TERM

http_get() {
    # http_get <url> <out>
    #
    # The transport restriction follows the URL scheme instead of being
    # unconditional. `https://` keeps it, so a github.com URL — which this
    # script only ever builds with an https:// base — is never fetched over
    # plaintext and never silently downgraded to http. `http://` drops it,
    # because a local install test serves the artifact over a plaintext
    # localhost server: with the restriction unconditional, RO_BASE_URL
    # could not reach a local server at all and the seam would be
    # decorative. Plaintext can only come from an explicit RO_BASE_URL;
    # the default base is always https.
    # The gate narrows rather than removes: even the http branch keeps an
    # explicit allowlist, with --proto-redir so a local server cannot bounce
    # the request to a wider scheme. The review that asked for this was
    # right — a bare `-fsSL` would follow a redirect anywhere, which for a
    # script whose whole job is downloading binaries is a liability.
    #
    # Two fixed words, written out rather than joined and split: a
    # `--proto '=x'` inside a variable is one argument to bash but can
    # arrive as two (or as `'=x'` with the quotes intact) once expanded,
    # which curl reports as "unrecognized protocol".
    local https_only=1
    case "$1" in
        http://*) https_only=0 ;;
    esac

    if command -v curl >/dev/null 2>&1; then
        if [ "$https_only" = 1 ]; then
            curl --proto '=https' --tlsv1.2 -fsSL --retry 3 --retry-delay 2 -o "$2" "$1"
        else
            curl --proto '=http,https' --proto-redir '=http,https' -fsSL --retry 3 --retry-delay 2 -o "$2" "$1"
        fi
    elif command -v wget >/dev/null 2>&1; then
        if [ "$https_only" = 1 ]; then
            wget --https-only --tries=3 -qO "$2" "$1"
        else
            wget --tries=3 -qO "$2" "$1"
        fi
    else
        err "neither curl nor wget is installed"
        exit 1
    fi
}

http_get_stdout() {
    if command -v curl >/dev/null 2>&1; then
        curl --proto '=https' --tlsv1.2 -fsSL --retry 3 --retry-delay 2 ${GITHUB_TOKEN:+-H "Authorization: Bearer $GITHUB_TOKEN"} "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget --https-only --tries=3 -qO- ${GITHUB_TOKEN:+--header="Authorization: Bearer $GITHUB_TOKEN"} "$1"
    else
        err "neither curl nor wget is installed"
        exit 1
    fi
}

# The tag of the latest release, resolved **without the GitHub API**.
#
# `releases/latest` answers with a redirect to `releases/tag/<tag>`, and
# following it costs no API quota. The API endpoint is unauthenticated by
# default and GitHub rate-limits it per source address — which is exactly
# what a CI runner and a user behind a shared NAT both are. The failure is a
# 403 on a script that had nothing to do with rate limits, and it is
# non-deterministic: two runners in the same workflow, one green and one
# red, on the same script.
#
# Returns non-zero if the shape is not what it expects, and the caller
# falls back to the API.
latest_tag_via_redirect() {
    command -v curl >/dev/null 2>&1 || return 1
    local effective
    effective="$(curl --proto '=https' --tlsv1.2 -fsSLI -o /dev/null \
        --retry 3 --retry-delay 2 \
        -w '%{url_effective}' \
        "https://github.com/${REPO}/releases/latest" 2>/dev/null)" || return 1
    effective="${effective%/}"
    case "$effective" in
        */releases/tag/*)
            printf '%s' "${effective##*/}"
            ;;
        *)
            # The redirect did not go where it should — a redirect to a
            # login page, or an HTML error page. Let the API try.
            return 1
            ;;
    esac
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
        # Redirect first, API second. The redirect needs no quota; the API
        # is the fallback for the machines that have no curl.
        tag="$(latest_tag_via_redirect 2>/dev/null || true)"
        if [ -z "$tag" ]; then
            tag="$(http_get_stdout "$api" 2>/dev/null \
                | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
                | head -n1)"
        fi
        if [ -z "$tag" ]; then
            err "could not resolve the latest release tag"
            err "GitHub may be rate-limiting this network; either set"
            err "  GITHUB_TOKEN=<token>    (only needs public read access)"
            err "or pin a version explicitly:"
            err "  RO_VERSION=v0.2.0 bash install.sh"
            exit 3
        fi

        # A tag-only release — CI still building, or a failed run — 404s on
        # the artifact URL. Worth catching early *when the API answers*.
        # When it does not answer, saying so and continuing is right: the
        # download reports a far more specific error than this can, and
        # failing here instead would break an install that was about to
        # work.
        local body
        if body="$(http_get_stdout "$api" 2>/dev/null)"; then
            case "$body" in
                *'"assets"'*'[]'*)
                    err "release ${tag} has no assets yet (CI may still be building)"
                    err "wait a few minutes and retry, or build from source:"
                    err "  git clone https://github.com/${REPO} && cd repo_orchestrator && cargo build --release"
                    exit 3
                    ;;
            esac
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

    # A bad RO_BASE_URL fails here with exit 3, naming the variable, before it
    # can surface as a curl/wget error on the first download.
    validate_base_url

    info "ro installer"
    info "repo:   https://github.com/${REPO}"
    info "user:   $(id -un 2>/dev/null || echo unknown)"

    local target tag archive_name checksum_url
    target="$(detect_target)"
    info "target: ${C_BOLD}${target}${C_RESET}"

    tag="$(resolve_version)"
    info "version: ${C_BOLD}${tag}${C_RESET}"

    # Every published release predates the `rfo` -> `ro` rename, so every
    # artifact on GitHub is `rfo-<target>.tar.xz` while this asks for
    # `ro-<target>.tar.xz` — and every install failed with a 404 whose
    # error pointed at the release rather than at the rename.
    #
    # The current name is tried first, so a release cut after the rename
    # behaves exactly as before, and the legacy branch disappears on its
    # own once there is no legacy release left to serve.
    # One base, one string, every URL. RO_BASE_URL replaces it wholesale so a
    # local install test can serve the archive; unset, the expansion is
    # byte-identical to the literal it replaced, so every real install
    # constructs exactly the URLs it always did. The archive and the
    # checksum below are both built from this variable — there is no second
    # expression for the checksum to disagree with.
    #
    # The checksum is not "fetched from somewhere else": `checksum_url` is
    # `<base>/<archive>.sha256`, so a RO_BASE_URL that names a directory
    # without the `.sha256` fails to fetch the checksum and the install dies
    # on a verification it could not perform. That is documented at the
    # BASE_URL seam above rather than invented as a past bug.
    local base="${BASE_URL:-https://github.com/${REPO}/releases/download/${tag}}"
    archive_name="${BIN}-${target}.tar.xz"
    checksum_url="${base}/${archive_name}.sha256"

    TMPDIR_RO="$(mktemp -d 2>/dev/null || mktemp -d -t ro-install)"

    info "downloading ${archive_name}"
    if ! http_get "${base}/${archive_name}" "${TMPDIR_RO}/${archive_name}"; then
        local legacy="rfo-${target}.tar.xz"
        if [ "$legacy" != "$archive_name" ] &&
           http_get "${base}/${legacy}" "${TMPDIR_RO}/${archive_name}"; then
            info "release ${tag} predates the rename; using ${legacy}"
            # The checksum is a separate asset under the same legacy name, so
            # pointing it at ${archive_name} 404s and the install dies on a
            # verification it could have performed.
            checksum_url="${base}/${legacy}.sha256"
        else
            err "failed to download ${base}/${archive_name}"
            err "check that release ${tag} exists and includes it for ${target}"
            exit 3
        fi
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
        # A pre-rename release ships a binary literally called `rfo`, and
        # the archive directory is named after it too. Without this the
        # download and the checksum both succeed and the install still
        # fails, on an archive we already hold.
        extracted="$(find "$TMPDIR_RO" -type f \( -name rfo -o -name "${BIN}" \) -perm -u+x 2>/dev/null | head -n1 || true)"
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
