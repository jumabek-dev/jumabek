#!/usr/bin/env bash
#
# Installs JumaBek on Linux and macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/jumabek-dev/jumabek/main/install.sh | bash
#   ./install.sh --version v1.0.0 --yes
#
set -euo pipefail

REPO="${JUMABEK_REPO:-jumabek-dev/jumabek}"
VERSION="latest"
ASSUME_YES=0

JB_HOME="$HOME/.jumabek"
BIN_DIR="$JB_HOME/bin"
SKILLS_DIR="$JB_HOME/skills"

say()  { printf '  %s\n' "$1"; }
step() { printf '  \033[36m%s\033[0m\n' "$1"; }
warn() { printf '  \033[33m%s\033[0m\n' "$1"; }
die()  { printf '  \033[31m%s\033[0m\n' "$1" >&2; exit 1; }

confirm() {
    local question="$1" default="${2:-n}"
    [ "$ASSUME_YES" = "1" ] && return 0
    [ -t 0 ] || { [ "$default" = "y" ]; return; }

    local suffix="[y/N]"
    [ "$default" = "y" ] && suffix="[Y/n]"

    printf '  %s %s ' "$question" "$suffix"
    read -r answer < /dev/tty || answer=""
    [ -z "$answer" ] && answer="$default"
    case "$answer" in [yYдД]|[yY]es|да) return 0 ;; *) return 1 ;; esac
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="$2"; shift 2 ;;
        --repo)    REPO="$2"; shift 2 ;;
        --yes|-y)  ASSUME_YES=1; shift ;;
        -h|--help)
            sed -n '3,7p' "$0" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        *) die "unknown option: $1" ;;
    esac
done

printf '\n  \033[36mJumaBek installer\033[0m\n\n'

# --- which build do we need --------------------------------------------------
case "$(uname -s)" in
    Linux)  os="unknown-linux-gnu" ;;
    Darwin) os="apple-darwin" ;;
    *)      die "unsupported system: $(uname -s) — build from source with cargo install" ;;
esac

case "$(uname -m)" in
    x86_64|amd64)  arch="x86_64" ;;
    arm64|aarch64) arch="aarch64" ;;
    *)             die "unsupported architecture: $(uname -m)" ;;
esac

if [ "$os" = "unknown-linux-gnu" ] && [ "$arch" = "aarch64" ]; then
    die "no linux arm64 build is published yet — build from source with cargo install"
fi

TARGET="$arch-$os"

fetch() {
    if command -v curl > /dev/null 2>&1; then
        curl -fsSL "$1"
    elif command -v wget > /dev/null 2>&1; then
        wget -qO- "$1"
    else
        die "neither curl nor wget is available"
    fi
}

download() {
    if command -v curl > /dev/null 2>&1; then
        curl -fsSL -o "$2" "$1"
    else
        wget -qO "$2" "$1"
    fi
}

if [ "$VERSION" = "latest" ]; then
    step "looking up the latest release"
    VERSION=$(fetch "https://api.github.com/repos/$REPO/releases/latest" \
        | grep -m1 '"tag_name"' | cut -d'"' -f4) || true
    [ -n "$VERSION" ] || die "cannot read the latest release from GitHub"
fi

ASSET="jumabek-$TARGET.tar.gz"
URL="https://github.com/$REPO/releases/download/$VERSION/$ASSET"
say "version $VERSION"

# --- download and verify -----------------------------------------------------
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

step "downloading $ASSET"
download "$URL" "$WORK/$ASSET" || die "download failed: $URL
  is there a release for $VERSION and $TARGET?"

if expected=$(fetch "$URL.sha256" 2>/dev/null | awk '{print $1}'); then
    if command -v sha256sum > /dev/null 2>&1; then
        actual=$(sha256sum "$WORK/$ASSET" | awk '{print $1}')
    else
        actual=$(shasum -a 256 "$WORK/$ASSET" | awk '{print $1}')
    fi

    [ "$expected" = "$actual" ] || die "checksum mismatch — the download is not what the release published"
    say "checksum ok"
else
    warn "no checksum published, continuing without verification"
fi

step "unpacking"
tar xzf "$WORK/$ASSET" -C "$WORK"
PAYLOAD="$WORK/jumabek-$TARGET"

# --- install -----------------------------------------------------------------
mkdir -p "$BIN_DIR" "$SKILLS_DIR"

install -m 755 "$PAYLOAD/jumabek" "$BIN_DIR/jumabek"
install -m 755 "$PAYLOAD/shell_executor" "$SKILLS_DIR/shell_executor"
step "installed to $BIN_DIR"

# Config and prompt belong to the user once they exist — never clobber them.
for file in config.toml prompt.md secrets.toml.example; do
    [ -f "$PAYLOAD/$file" ] || continue
    if [ -f "$JB_HOME/$file" ]; then
        say "kept your existing $file"
    else
        cp "$PAYLOAD/$file" "$JB_HOME/$file"
        say "created $file"
    fi
done

# --- PATH --------------------------------------------------------------------
case ":$PATH:" in
    *":$BIN_DIR:"*) say "PATH already contains $BIN_DIR" ;;
    *)
        line="export PATH=\"\$PATH:$BIN_DIR\""
        added=0
        for rc in "$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.profile"; do
            [ -f "$rc" ] || continue
            grep -qF "$BIN_DIR" "$rc" && { added=1; continue; }
            printf '\n# JumaBek\n%s\n' "$line" >> "$rc"
            step "added $BIN_DIR to PATH in $(basename "$rc")"
            added=1
        done

        if [ "$added" = "0" ]; then
            warn "add this to your shell profile yourself:"
            say "$line"
        else
            say "restart your shell for it to take effect"
        fi
        ;;
esac
export PATH="$PATH:$BIN_DIR"

# --- the LLM endpoint --------------------------------------------------------
printf '\n  \033[36mJumaBek needs an OpenAI-compatible endpoint to talk to.\033[0m\n'
say "Point [llm].base_uri in $JB_HOME/config.toml at whichever you use:"
say "  a local runner   Ollama, LM Studio, llama.cpp  (these want no API key)"
say "  a router         one endpoint in front of several providers"
say "  a provider       directly, with its own key"
printf '\n'

# --- report ------------------------------------------------------------------
printf '\n'
step "checking the setup"
"$BIN_DIR/jumabek" doctor || true

printf '\n  \033[36mRun:  jumabek\033[0m\n'
printf '    \033[90mAn endpoint that wants an API key: export JUMABEK_API_KEY="your-key"\033[0m\n'
printf '    \033[90mor put it in %s/secrets.toml\033[0m\n\n' "$JB_HOME"
