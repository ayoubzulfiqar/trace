#!/usr/bin/env bash
#
# One-line installer for trace.
#
#   curl -fsSL https://raw.githubusercontent.com/ayoubzulfiqar/trace/main/install.sh | sh
#
# Downloads a pre-compiled trace binary for your OS/architecture, installs it
# to /usr/local/bin (or ~/.local/bin if not root), and runs `trace setup`
# to auto-register the MCP server with any discovered AI agents.
#
set -euo pipefail

REPO="ayoubzulfiqar/trace"
BINARY="trace"

# ── Detect OS ────────────────────────────────────────────────────────────────
detect_os() {
    local kernel
    kernel="$(uname -s)"
    case "$kernel" in
        Linux*)  echo "linux" ;;
        Darwin*) echo "macos" ;;
        MINGW*|MSYS*|CYGWIN*) echo "windows" ;;
        *)
            echo "Unsupported OS: $kernel" >&2
            exit 1
            ;;
    esac
}

# ── Detect architecture ──────────────────────────────────────────────────────
detect_arch() {
    local machine
    machine="$(uname -m)"
    case "$machine" in
        x86_64|amd64) echo "x86_64" ;;
        aarch64|arm64) echo "aarch64" ;;
        *)
            echo "Unsupported architecture: $machine" >&2
            exit 1
            ;;
    esac
}

# ── Resolve install directory ────────────────────────────────────────────────
resolve_install_dir() {
    if [ -w "/usr/local/bin" ]; then
        echo "/usr/local/bin"
    else
        echo "${HOME}/.local/bin"
    fi
}

# ── Main ─────────────────────────────────────────────────────────────────────
main() {
    local os arch install_dir binary_path version
    os="$(detect_os)"
    arch="$(detect_arch)"
    install_dir="$(resolve_install_dir)"
    version="${TRACE_VERSION:-latest}"

    mkdir -p "$install_dir"
    binary_path="${install_dir}/${BINARY}"

    echo "Installing trace..."
    echo "  OS:          $os"
    echo "  Architecture: $arch"
    echo "  Install dir:  $install_dir"
    echo "  Version:      $version"

    local release_tag
    if [ "$version" = "latest" ]; then
        release_tag="$(curl -fsSL \
            "https://api.github.com/repos/${REPO}/releases/latest" \
            | grep -o '"tag_name": *"[^"]*"' | head -1 | cut -d'"' -f4)"
    else
        release_tag="v${version}"
    fi

    if [ -z "$release_tag" ]; then
        echo "Error: could not determine latest release tag" >&2
        exit 1
    fi

    local asset_name target
    case "$os" in
        linux)   asset_name="trace-${release_tag}-linux-${arch}.tar.gz" ;;
        macos)   asset_name="trace-${release_tag}-macos-${arch}.tar.gz" ;;
        windows) asset_name="trace-${release_tag}-windows-${arch}.zip" ;;
    esac

    local download_url="https://github.com/${REPO}/releases/download/${release_tag}/${asset_name}"
    echo "  Downloading: $download_url"

    local tmp_dir
    tmp_dir="$(mktemp -d)"
    trap 'rm -rf "$tmp_dir"' EXIT

    if ! curl -fsSL "$download_url" -o "${tmp_dir}/${asset_name}"; then
        echo "Error: download failed" >&2
        exit 1
    fi

    case "$os" in
        linux|macos)
            tar xzf "${tmp_dir}/${asset_name}" -C "$tmp_dir"
            cp "${tmp_dir}/${BINARY}" "$binary_path"
            chmod +x "$binary_path"
            ;;
        windows)
            unzip -o "${tmp_dir}/${asset_name}" -d "$tmp_dir"
            # Strip .exe if present
            cp "${tmp_dir}/${BINARY}.exe" "${binary_path}.exe"
            chmod +x "${binary_path}.exe"
            binary_path="${binary_path}.exe"
            ;;
    esac

    echo ""
    echo "✓ trace installed at: ${binary_path}"

    if ! echo "$PATH" | tr ':' '\n' | grep -q "$(dirname "$binary_path")"; then
        echo ""
        echo "⚠  Note: $(dirname "$binary_path") is not in your PATH."
        echo "  Add the following to your shell config (~/.bashrc, ~/.zshrc, etc.):"
        echo "    export PATH=\"\$(dirname "$binary_path"):\$PATH\""
    fi

    echo ""
    echo "Running auto-discovery and MCP registration..."
    "$binary_path" setup || true

    echo ""
    echo "Installing system service (background daemon)..."
    "$binary_path" service install || true

    echo ""
    echo "✓ Installation complete."
    echo "  Run 'trace scan <project>' to index a codebase."
    echo "  Run 'trace serve <project>' to start the MCP server (connects to daemon)."
    echo "  Run 'trace daemon <project>' to start the background daemon directly."
}

main "$@"
