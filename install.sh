#!/usr/bin/env bash
# Orgos Quick Installer - Get running in under 1 minute
# Usage: curl -fsSL https://orgos.cc/install.sh | bash
set -euo pipefail

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

info() { echo -e "${BLUE}[INFO]${NC} $*"; }
success() { echo -e "${GREEN}[OK]${NC} $*"; }
warn() { echo -e "${YELLOW}[WARN]${NC} $*"; }
error() { echo -e "${RED}[ERROR]${NC} $*" >&2; }

# Configuration
INSTALL_DIR="${ORGOS_INSTALL_DIR:-$HOME/.orgos}"
PORT="${ORGOS_PORT:-8888}"
VERSION="${ORGOS_VERSION:-latest}"

echo ""
echo -e "${GREEN}╔═══════════════════════════════════════════╗${NC}"
echo -e "${GREEN}║     Orgos - Private AI Search Engine      ║${NC}"
echo -e "${GREEN}║        Quick Install (< 1 minute)         ║${NC}"
echo -e "${GREEN}╚═══════════════════════════════════════════╝${NC}"
echo ""

# Detect OS and architecture
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64) ARCH="x86_64" ;;
  aarch64|arm64) ARCH="aarch64" ;;
  *) error "Unsupported architecture: $ARCH"; exit 1 ;;
esac

info "Detected: $OS-$ARCH"

# Create install directory
mkdir -p "$INSTALL_DIR"
cd "$INSTALL_DIR"

# Check if already installed
if [[ -f "$INSTALL_DIR/orgos" ]]; then
  warn "Orgos already installed at $INSTALL_DIR"
  read -p "Reinstall? [y/N] " -n 1 -r
  echo
  [[ ! $REPLY =~ ^[Yy]$ ]] && { info "Keeping existing installation."; exit 0; }
fi

# Download binary or build from source
download_binary() {
  local url="https://github.com/orgos/releases/download/${VERSION}/orgos-${OS}-${ARCH}.tar.gz"
  info "Downloading Orgos..."
  if command -v curl &>/dev/null; then
    curl -fsSL "$url" | tar -xz
  elif command -v wget &>/dev/null; then
    wget -qO- "$url" | tar -xz
  else
    return 1
  fi
}

build_from_source() {
  info "Building from source (this may take 2-3 minutes)..."
  if ! command -v cargo &>/dev/null; then
    warn "Rust not found. Installing Rust..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
  fi

  if [[ -d "metasearch" ]]; then
    cd metasearch && git pull
  else
    git clone --depth 1 https://github.com/user/metasearch.git
    cd metasearch
  fi

  cargo build --release
  cp target/release/metasearch "$INSTALL_DIR/orgos"
  cd "$INSTALL_DIR"
}

# Try binary first, fallback to source
if ! download_binary 2>/dev/null; then
  warn "Pre-built binary not available, building from source..."
  build_from_source
fi

# Create default settings if not exists
if [[ ! -f "$INSTALL_DIR/settings.yml" ]]; then
  info "Creating default settings..."
  cat > "$INSTALL_DIR/settings.yml" << 'SETTINGS'
# Orgos Configuration - Works out of the box!
# Edit this file to customize your instance.

branding:
  app_name: "Orgos"

server:
  bind_address: "0.0.0.0"
  port: 8888
  max_connections: 100
  image_proxy: true
  cache_backend: "memory"
  cache_ttl_secs: 300
  request_timeout_secs: 10

search:
  default_lang: "en"
  default_language: "auto"
  safe_search: 0
  autocomplete: true
  formats:
    - json
    - html

ai:
  enabled: true
  base_url: "http://127.0.0.1:11434"
  model: "gemma3:4b"
  answer: true
  expand: false
  rerank: false
  timeout_secs: 60
  answer_language: "auto"

# Built-in search engines (all enabled by default)
engines:
  - name: googlenews
    enabled: true
    weight: 1.5
    categories: [general, news]
  - name: bing
    enabled: true
    weight: 1.2
    categories: [general]
  - name: brave
    enabled: true
    weight: 1.0
    categories: [general]
  - name: wikipedia
    enabled: true
    weight: 1.0
    categories: [general]
  - name: wikidata
    enabled: true
    weight: 0.8
    categories: [general]
  - name: duckduckgo
    enabled: true
    weight: 1.0
    categories: [general]
  - name: bing_images
    enabled: true
    weight: 1.0
    categories: [images]
  - name: brave_images
    enabled: true
    weight: 1.0
    categories: [images]
  - name: duckduckgo_images
    enabled: true
    weight: 1.0
    categories: [images]
SETTINGS
fi

# Create start script
cat > "$INSTALL_DIR/start.sh" << 'START'
#!/usr/bin/env bash
cd "$(dirname "$0")"
PORT="${ORGOS_PORT:-8888}"

# Check if already running
if curl -sf "http://127.0.0.1:$PORT/healthz" &>/dev/null; then
  echo "Orgos already running at http://127.0.0.1:$PORT"
  exit 0
fi

# Start server
echo "Starting Orgos on port $PORT..."
./orgos &
PID=$!
echo $PID > .orgos.pid

# Wait for ready
for i in {1..30}; do
  if curl -sf "http://127.0.0.1:$PORT/healthz" &>/dev/null; then
    echo "✓ Orgos ready at http://127.0.0.1:$PORT"

    # Open browser
    case "$(uname -s)" in
      Darwin) open "http://127.0.0.1:$PORT" ;;
      Linux) xdg-open "http://127.0.0.1:$PORT" 2>/dev/null || true ;;
    esac

    exit 0
  fi
  sleep 0.5
done

echo "Failed to start. Check logs."
exit 1
START
chmod +x "$INSTALL_DIR/start.sh"

# Create stop script
cat > "$INSTALL_DIR/stop.sh" << 'STOP'
#!/usr/bin/env bash
cd "$(dirname "$0")"
if [[ -f .orgos.pid ]]; then
  PID=$(cat .orgos.pid)
  if kill -0 "$PID" 2>/dev/null; then
    kill "$PID"
    echo "Orgos stopped."
  fi
  rm -f .orgos.pid
else
  pkill -f "orgos" 2>/dev/null || true
  echo "Orgos stopped."
fi
STOP
chmod +x "$INSTALL_DIR/stop.sh"

# Add to PATH (optional)
SHELL_RC=""
if [[ -f "$HOME/.zshrc" ]]; then
  SHELL_RC="$HOME/.zshrc"
elif [[ -f "$HOME/.bashrc" ]]; then
  SHELL_RC="$HOME/.bashrc"
fi

if [[ -n "$SHELL_RC" ]]; then
  if ! grep -q "ORGOS_INSTALL_DIR" "$SHELL_RC" 2>/dev/null; then
    echo "" >> "$SHELL_RC"
    echo "# Orgos" >> "$SHELL_RC"
    echo "export PATH=\"\$PATH:$INSTALL_DIR\"" >> "$SHELL_RC"
  fi
fi

echo ""
success "Installation complete!"
echo ""
echo -e "  ${GREEN}Start:${NC}  $INSTALL_DIR/start.sh"
echo -e "  ${GREEN}Stop:${NC}   $INSTALL_DIR/stop.sh"
echo -e "  ${GREEN}Config:${NC} $INSTALL_DIR/settings.yml"
echo ""

# Prompt to start
read -p "Start Orgos now? [Y/n] " -n 1 -r
echo
if [[ ! $REPLY =~ ^[Nn]$ ]]; then
  "$INSTALL_DIR/start.sh"
fi
