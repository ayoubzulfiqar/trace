#!/bin/sh
#
# One-line installer for trace.
#
#   curl -fsSL https://raw.githubusercontent.com/ayoubzulfiqar/trace/main/install.sh | sh
#
# Downloads the pre-compiled trace binary for this OS/architecture, verifies
# its SHA-256 checksum, installs it to /usr/local/bin (or ~/.local/bin when
# that is not writable), and runs `trace setup` to register the MCP server
# with every AI agent it finds.
#
# Environment:
#   TRACE_VERSION      release to install, e.g. 2.6.9 (default: latest)
#   TRACE_INSTALL_DIR  install directory (default: /usr/local/bin or ~/.local/bin)
#   TRACE_NO_SETUP=1   skip `trace setup`
#   TRACE_PACKAGE=1    Linux: install the native package instead (.deb on
#                      Debian/Ubuntu, .rpm on Fedora/RHEL, .pkg.tar.zst on
#                      Arch) through apt/dnf/pacman — needs root or sudo
#
# POSIX sh: works with dash/busybox as well as bash.
set -eu

REPO="ayoubzulfiqar/trace"
BINARY="trace"
TMP_DIR=""

cleanup() {
    if [ -n "$TMP_DIR" ]; then
        rm -rf "$TMP_DIR"
    fi
}
trap cleanup EXIT INT TERM

say() { printf '%s\n' "$*"; }
err() { printf 'error: %s\n' "$*" >&2; exit 1; }

have() { command -v "$1" >/dev/null 2>&1; }

# fetch URL DEST — download with curl or wget.
fetch() {
    if have curl; then
        curl -fsSL --retry 3 "$1" -o "$2"
    elif have wget; then
        wget -q -O "$2" "$1"
    else
        err "curl or wget is required"
    fi
}

detect_os() {
    case "$(uname -s)" in
        Linux*) echo "linux" ;;
        Darwin*) echo "macos" ;;
        MINGW* | MSYS* | CYGWIN*) echo "windows" ;;
        *) err "unsupported OS: $(uname -s)" ;;
    esac
}

detect_arch() {
    case "$(uname -m)" in
        x86_64 | amd64) echo "x86_64" ;;
        aarch64 | arm64) echo "aarch64" ;;
        *) err "unsupported architecture: $(uname -m)" ;;
    esac
}

resolve_install_dir() {
    if [ -n "${TRACE_INSTALL_DIR:-}" ]; then
        echo "$TRACE_INSTALL_DIR"
    elif [ -d /usr/local/bin ] && [ -w /usr/local/bin ]; then
        echo "/usr/local/bin"
    else
        echo "${HOME}/.local/bin"
    fi
}

latest_tag() {
    api="${TMP_DIR}/latest.json"
    fetch "https://api.github.com/repos/${REPO}/releases/latest" "$api" ||
        err "could not query the latest release"
    sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$api" | head -n 1
}

# verify FILE SUMFILE — check a "<sha256>  <name>" checksum file.
verify() {
    expected="$(awk '{print $1; exit}' "$2")"
    [ -n "$expected" ] || err "empty checksum file"
    if have sha256sum; then
        actual="$(sha256sum "$1" | awk '{print $1}')"
    elif have shasum; then
        actual="$(shasum -a 256 "$1" | awk '{print $1}')"
    else
        say "  ⚠ no sha256sum/shasum found; skipping checksum verification"
        return 0
    fi
    [ "$expected" = "$actual" ] || err "checksum mismatch for $(basename "$1")"
    say "  Checksum OK"
}

# Linux distribution family from /etc/os-release: deb, rpm, arch or empty.
distro_family() {
    [ -r /etc/os-release ] || return 0
    # shellcheck disable=SC1091
    ids="$(. /etc/os-release && echo " ${ID:-} ${ID_LIKE:-} ")"
    case "$ids" in
        *" debian "* | *" ubuntu "*) echo "deb" ;;
        *" fedora "* | *" rhel "* | *" centos "*) echo "rpm" ;;
        *" arch "*) echo "arch" ;;
    esac
}

as_root() {
    if [ "$(id -u)" = "0" ]; then
        "$@"
    elif have sudo; then
        sudo "$@"
    else
        err "installing a system package needs root or sudo"
    fi
}

# download_verified URL FILE — fetch into TMP_DIR and check its .sha256.
download_verified() {
    fetch "$1" "${TMP_DIR}/$2" || return 1
    if fetch "$1.sha256" "${TMP_DIR}/$2.sha256" 2>/dev/null; then
        verify "${TMP_DIR}/$2" "${TMP_DIR}/$2.sha256"
    else
        say "  ⚠ no checksum published for $2; skipping verification"
    fi
}

# install_package BASE_URL TAG ARCH — native package; returns 1 when no
# package is published for this distribution/architecture.
install_package() {
    family="$(distro_family)"
    ver="${2#v}"
    case "$family:$3" in
        deb:x86_64) pkg="${BINARY}_${ver}-1_amd64.deb" ;;
        deb:aarch64) pkg="${BINARY}_${ver}-1_arm64.deb" ;;
        rpm:*) pkg="${BINARY}-${ver}-1.$3.rpm" ;;
        arch:x86_64) pkg="${BINARY}-${ver}-1-x86_64.pkg.tar.zst" ;;
        *) return 1 ;;
    esac
    say "  Package:     ${pkg}"
    download_verified "$1/${pkg}" "$pkg" || return 1
    case "$family" in
        deb)
            if have apt-get; then
                as_root apt-get install -y "${TMP_DIR}/${pkg}"
            else
                as_root dpkg -i "${TMP_DIR}/${pkg}"
            fi
            ;;
        rpm)
            if have dnf; then
                as_root dnf install -y "${TMP_DIR}/${pkg}"
            else
                as_root rpm -Uvh "${TMP_DIR}/${pkg}"
            fi
            ;;
        arch) as_root pacman -U --noconfirm "${TMP_DIR}/${pkg}" ;;
    esac
}

on_path() {
    case ":${PATH}:" in
        *":$1:"*) return 0 ;;
        *) return 1 ;;
    esac
}

main() {
    os="$(detect_os)"
    arch="$(detect_arch)"
    install_dir="$(resolve_install_dir)"
    version="${TRACE_VERSION:-latest}"
    TMP_DIR="$(mktemp -d 2>/dev/null || mktemp -d -t trace-install)"

    if [ "$version" = "latest" ]; then
        tag="$(latest_tag)"
        [ -n "$tag" ] || err "could not determine the latest release tag"
    else
        case "$version" in
            v*) tag="$version" ;;
            *) tag="v${version}" ;;
        esac
    fi

    base="https://github.com/${REPO}/releases/download/${tag}"
    say "Installing trace ${tag}"
    say "  OS/arch:     ${os}/${arch}"

    dest=""
    if [ "$os" = "linux" ] && [ "${TRACE_PACKAGE:-0}" = "1" ]; then
        if install_package "$base" "$tag" "$arch"; then
            dest="/usr/bin/${BINARY}"
            install_dir="/usr/bin"
            found="$(command -v "$BINARY" || true)"
            if [ -n "$found" ] && [ "$found" != "$dest" ]; then
                say "  ⚠ ${found} shadows the packaged ${dest} on your PATH; remove it."
            fi
        else
            say "  No native package for this distribution/architecture; using the tarball."
        fi
    fi

    if [ -z "$dest" ]; then
        case "$os" in
            windows) asset="${BINARY}-${tag}-${os}-${arch}.zip" ;;
            *) asset="${BINARY}-${tag}-${os}-${arch}.tar.gz" ;;
        esac
        say "  Install dir: ${install_dir}"
        say "  Downloading: ${base}/${asset}"
        download_verified "${base}/${asset}" "$asset" || err "download failed"

        mkdir -p "$install_dir"
        case "$os" in
            windows)
                have unzip || err "unzip is required"
                unzip -oq "${TMP_DIR}/${asset}" -d "$TMP_DIR"
                src="${TMP_DIR}/${BINARY}.exe"
                dest="${install_dir}/${BINARY}.exe"
                ;;
            *)
                tar -xzf "${TMP_DIR}/${asset}" -C "$TMP_DIR"
                src="${TMP_DIR}/${BINARY}"
                dest="${install_dir}/${BINARY}"
                ;;
        esac
        [ -f "$src" ] || err "archive did not contain ${BINARY}"
        # Install via a temp name + rename so a running daemon's binary is
        # never truncated in place.
        cp "$src" "${dest}.new"
        chmod 755 "${dest}.new"
        mv -f "${dest}.new" "$dest"
    fi

    if ! "$dest" --version >/dev/null 2>&1; then
        err "the installed binary does not run on this system (Linux release binaries are built on Ubuntu 22.04 against glibc; on older or musl-based systems build from source with cargo)"
    fi
    say ""
    say "✓ trace installed at ${dest} ($("$dest" --version))"

    if ! on_path "$install_dir"; then
        say ""
        say "⚠ ${install_dir} is not on your PATH. Add this to your shell profile:"
        say "    export PATH=\"${install_dir}:\$PATH\""
    fi

    if [ "${TRACE_NO_SETUP:-0}" != "1" ]; then
        say ""
        say "Registering trace with installed AI agents..."
        "$dest" setup || say "  (setup reported problems; re-run \`trace setup\` after fixing them)"
    fi

    say ""
    say "✓ Installation complete."
    say "  trace status                    show index/rules/decisions/daemon state for a project"
    say "  trace scan <project>            index a codebase"
    say "  trace check                     enforce architectural rules (CI / git hooks)"
    say "  trace service install <project> keep a project's daemon running at login (optional;"
    say "                                  agents start it on demand via \`trace serve\`)"
}

main "$@"
