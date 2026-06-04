#!/bin/sh
#
# next-socks5 one-shot installer. POSIX sh (no bash required).
#
# Installs and starts a next-socks5 SOCKS5 server, either as a native binary
# (downloaded from GitHub Releases + a systemd service) or via Docker Compose.
#
# Examples:
#   ./install.sh                          # binary install, auth on (random creds), random port
#   ./install.sh --method docker          # docker compose, auth on (random creds), random port
#   ./install.sh --no-auth --port 1080    # no auth, fixed port
#   ./install.sh --auth --user bob --pass s3cret --port 1080
#
set -eu

# --- Constants ----------------------------------------------------------------
REPO="ZingerLittleBee/next-socks5"
IMAGE="ghcr.io/zingerlittlebee/next-socks5"
BIN_NAME="next-socks5"

# --- Defaults -----------------------------------------------------------------
METHOD="binary"           # binary | docker
AUTH="on"                 # on | off  (on => username/password)
USERNAME=""               # auto-generated when AUTH=on and unset
PASSWORD=""               # auto-generated when AUTH=on and unset
PORT=""                   # random free port when unset
LISTEN_ADDR="0.0.0.0"     # bind address inside config
VERSION="latest"          # release tag (e.g. v0.1.0) or "latest"
BIN_DIR="/usr/local/bin"  # binary install dir
DEPLOY_DIR="./next-socks5-deploy"   # docker compose dir
NO_SERVICE="${NO_SERVICE:-no}"      # skip init service setup (env or --no-service)
STARTED="no"                        # flipped to "yes" once a service is started

# --- Logging ------------------------------------------------------------------
log()  { printf '\033[0;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[0;33m[warn]\033[0m %s\n' "$*" >&2; }
err()  { printf '\033[0;31m[error]\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'EOF'
next-socks5 installer

Usage: install.sh [options]

  --method <binary|docker>  Install as a native binary (default) or via Docker Compose.
  --auth                    Enable username/password auth (default).
  --no-auth                 Disable auth (open proxy — use with care).
  --user <name>             Auth username (auth mode; random if omitted).
  --pass <password>         Auth password (auth mode; random if omitted).
  --port <port>             Listen port (random free port if omitted).
  --listen <addr>           Bind address (default 0.0.0.0).
  --version <tag>           Release version, e.g. v0.1.0 (default: latest).
  --bin-dir <dir>           Binary install dir (default /usr/local/bin).
  --dir <dir>               Docker deploy dir (default ./next-socks5-deploy).
  --no-service              Install binary + config only; do not set up/start a
                            systemd/OpenRC service (also via env NO_SERVICE=1).
  -h, --help                Show this help.

Notes:
  * Binary install targets Linux (musl static builds: x86_64 / aarch64) and sets
    up a systemd or OpenRC service. If neither is present, the binary + config
    are installed but NOT started (and won't auto-start after a reboot) — start
    it manually, or use --method docker for a self-restarting container.
  * Docker install uses host networking so UDP ASSOCIATE works correctly
    (Linux hosts; Docker Desktop on macOS/Windows does not support host mode).
EOF
}

# --- Helpers ------------------------------------------------------------------
# Clean up temp dirs on exit (sh has no function-scoped RETURN trap).
_TMP=""
cleanup() { [ -n "$_TMP" ] && rm -rf "$_TMP"; return 0; }
trap cleanup EXIT INT TERM

need_cmd() { command -v "$1" >/dev/null 2>&1 || err "required command not found: $1"; }

# Run a command as root when not already root.
SUDO=""
ensure_sudo() {
  if [ "$(id -u)" -ne 0 ]; then
    command -v sudo >/dev/null 2>&1 || err "this step needs root; install sudo or run as root"
    SUDO="sudo"
  fi
}

# Generate a random alphanumeric secret of length $1 (default 16).
gen_secret() {
  local n="${1:-16}"
  if command -v openssl >/dev/null 2>&1; then
    openssl rand -hex "$n" | cut -c "1-$n"
  else
    LC_ALL=C tr -dc 'a-zA-Z0-9' < /dev/urandom 2>/dev/null | head -c "$n" || true
  fi
}

# Map the host architecture to a release target triple.
detect_target() {
  case "$(uname -m)" in
    x86_64|amd64)   echo "x86_64-unknown-linux-musl" ;;
    aarch64|arm64)  echo "aarch64-unknown-linux-musl" ;;
    *) err "unsupported architecture: $(uname -m) (only x86_64 / aarch64 are published)" ;;
  esac
}

# True if a TCP port is already in use.
port_in_use() {
  local p="$1"
  if command -v ss >/dev/null 2>&1; then
    ss -tuln 2>/dev/null | grep -qE "[:.]${p}([[:space:]]|$)"
  elif command -v lsof >/dev/null 2>&1; then
    lsof -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1
  elif command -v nc >/dev/null 2>&1; then
    nc -z 127.0.0.1 "$p" >/dev/null 2>&1
  else
    return 1   # cannot check — assume free
  fi
}

# Echo a pseudo-random integer in [20000, 40000) via /dev/urandom (sh has no $RANDOM).
rand_port() {
  local n
  n="$(od -An -N2 -tu2 /dev/urandom 2>/dev/null | tr -d ' ')"
  [ -n "$n" ] || n="$$"
  echo $(( (n % 20000) + 20000 ))
}

# Pick a random free port in [20000, 40000).
find_free_port() {
  local p i
  i=0
  while [ "$i" -lt 100 ]; do
    p="$(rand_port)"
    if ! port_in_use "$p"; then echo "$p"; return 0; fi
    i=$((i + 1))
  done
  err "could not find a free port after 100 attempts"
}

# Emit the config.toml contents to stdout.
render_config() {
  echo "listen = \"${LISTEN_ADDR}:${PORT}\""
  echo ""
  echo "[auth]"
  if [ "$AUTH" = "on" ]; then
    echo "method = \"password\""
    echo "[[auth.users]]"
    echo "username = \"${USERNAME}\""
    echo "password = \"${PASSWORD}\""
  else
    echo "method = \"none\""
  fi
  echo ""
  echo "[timeouts]"
  echo "connect_ms = 10000"
  echo "tcp_idle_ms = 300000"
  echo "udp_idle_ms = 60000"
}

# Resolve the public IP via api.ipify.org (used for the shown proxy URL).
get_public_ip() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsS -m 5 https://api.ipify.org 2>/dev/null
  elif command -v wget >/dev/null 2>&1; then
    wget -qO- -T 5 https://api.ipify.org 2>/dev/null
  fi
}

# Print "label:" then its value on the next line, then a blank line, so the
# value sits alone on one line and is easy to copy.
field() { printf '%s:\n%s\n\n' "$1" "$2"; }

# Explain how to start the server by hand (used when no service is set up).
print_manual_start() {
  STARTED="no"
  warn "service NOT started; start it manually with:"
  warn "  ${BIN_DIR}/${BIN_NAME} serve --no-tui --config /etc/next-socks5/config.toml"
  warn "without systemd/OpenRC it will NOT auto-start after a reboot"
  MANAGE_HINT="start: ${BIN_DIR}/${BIN_NAME} serve --no-tui --config /etc/next-socks5/config.toml"
}

# --- Argument parsing ---------------------------------------------------------
while [ $# -gt 0 ]; do
  case "$1" in
    --method)   METHOD="${2:?--method needs a value}"; shift 2 ;;
    --auth)     AUTH="on"; shift ;;
    --no-auth)  AUTH="off"; shift ;;
    --user)     USERNAME="${2:?--user needs a value}"; shift 2 ;;
    --pass)     PASSWORD="${2:?--pass needs a value}"; shift 2 ;;
    --port)     PORT="${2:?--port needs a value}"; shift 2 ;;
    --listen)   LISTEN_ADDR="${2:?--listen needs a value}"; shift 2 ;;
    --version)  VERSION="${2:?--version needs a value}"; shift 2 ;;
    --bin-dir)  BIN_DIR="${2:?--bin-dir needs a value}"; shift 2 ;;
    --dir)      DEPLOY_DIR="${2:?--dir needs a value}"; shift 2 ;;
    --no-service) NO_SERVICE="yes"; shift ;;
    -h|--help)  usage; exit 0 ;;
    *) err "unknown option: $1 (see --help)" ;;
  esac
done

# --- Resolve dynamic defaults -------------------------------------------------
case "$METHOD" in binary|docker) ;; *) err "--method must be 'binary' or 'docker'";; esac
case "$NO_SERVICE" in 1|y|yes|true|on) NO_SERVICE="yes" ;; *) NO_SERVICE="no" ;; esac

if [ -z "$PORT" ]; then
  PORT="$(find_free_port)"
  log "selected random free port: $PORT"
fi
case "$PORT" in *[!0-9]*) err "--port must be numeric";; esac

if [ "$AUTH" = "on" ]; then
  [ -n "$USERNAME" ] || { USERNAME="user_$(gen_secret 6)"; log "generated username: $USERNAME"; }
  [ -n "$PASSWORD" ] || { PASSWORD="$(gen_secret 20)"; log "generated password: $PASSWORD"; }
else
  if [ -n "$USERNAME" ] || [ -n "$PASSWORD" ]; then
    warn "--no-auth set; ignoring provided --user/--pass"
  fi
fi

# --- Binary install -----------------------------------------------------------
install_binary() {
  [ "$(uname -s)" = "Linux" ] || err "binary install supports Linux only; try --method docker"
  need_cmd curl
  need_cmd tar
  local target url tmp
  target="$(detect_target)"
  if [ "$VERSION" = "latest" ]; then
    url="https://github.com/${REPO}/releases/latest/download/${BIN_NAME}-${target}.tar.gz"
  else
    url="https://github.com/${REPO}/releases/download/${VERSION}/${BIN_NAME}-${target}.tar.gz"
  fi

  tmp="$(mktemp -d)"
  _TMP="$tmp"   # removed by the global EXIT trap (sh has no RETURN trap)
  log "downloading ${BIN_NAME} (${target}, ${VERSION})"
  curl -fL --retry 3 -o "$tmp/pkg.tar.gz" "$url" \
    || err "download failed: $url (is the release published yet?)"
  tar xzf "$tmp/pkg.tar.gz" -C "$tmp"

  local src
  src="$(find "$tmp" -type f -name "$BIN_NAME" | head -n1)"
  [ -n "$src" ] || err "binary not found in downloaded archive"
  chmod +x "$src"

  ensure_sudo
  log "installing binary to ${BIN_DIR}/${BIN_NAME}"
  $SUDO install -d "$BIN_DIR"
  $SUDO install -m 0755 "$src" "${BIN_DIR}/${BIN_NAME}"

  log "writing config to /etc/next-socks5/config.toml"
  $SUDO install -d /etc/next-socks5
  render_config | $SUDO tee /etc/next-socks5/config.toml >/dev/null
  $SUDO chmod 0640 /etc/next-socks5/config.toml

  # --- Service setup (skipped with --no-service) ---
  if [ "$NO_SERVICE" = "yes" ]; then
    warn "--no-service: installed binary + config only"
    print_manual_start
  elif command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
    log "installing systemd service: next-socks5.service"
    $SUDO tee /etc/systemd/system/next-socks5.service >/dev/null <<EOF
[Unit]
Description=next-socks5 SOCKS5 server
After=network.target

[Service]
# Read the config as root and hand it (mode 0400) to the DynamicUser via the
# systemd credentials store, so the random service UID can read it without
# widening the file's own permissions (the config holds the password).
LoadCredential=config:/etc/next-socks5/config.toml
ExecStart=${BIN_DIR}/${BIN_NAME} serve --no-tui --config %d/config
Restart=on-failure
DynamicUser=yes
# Create /run/next-socks5 (writable by the DynamicUser, cleaned up on stop) for
# the admin/attach Unix socket. 0710: owner rw, same-group may enter, others
# none; root can still bypass DAC to attach.
RuntimeDirectory=next-socks5
RuntimeDirectoryMode=0710
AmbientCapabilities=CAP_NET_BIND_SERVICE
NoNewPrivileges=yes

[Install]
WantedBy=multi-user.target
EOF
    $SUDO systemctl daemon-reload
    $SUDO systemctl enable next-socks5.service
    # restart (not `enable --now`) so a reinstall actually reloads the new
    # unit + config; `enable --now` does NOT restart an already-running service.
    $SUDO systemctl restart next-socks5.service
    # A port clash (e.g. a Docker install already bound to this port) makes the
    # unit crash-loop; confirm it is actually active rather than reporting success.
    sleep 1
    if ! $SUDO systemctl is-active --quiet next-socks5.service; then
      warn "service failed to start — recent logs:"
      $SUDO journalctl -u next-socks5 -n 20 --no-pager >&2 2>/dev/null || true
      err "next-socks5.service is not active (often: port ${PORT} already in use — check 'ss -tlnp | grep ${PORT}')"
    fi
    STARTED="yes"
    MANAGE_HINT="systemctl status next-socks5 | journalctl -u next-socks5 -f"
  elif command -v rc-update >/dev/null 2>&1 && command -v rc-service >/dev/null 2>&1 \
       && { [ -d /run/openrc ] || rc-status >/dev/null 2>&1; }; then
    # OpenRC (Alpine and other OpenRC-based distros).
    log "installing OpenRC service: next-socks5"
    $SUDO tee /etc/init.d/next-socks5 >/dev/null <<EOF
#!/sbin/openrc-run

name="next-socks5"
description="next-socks5 SOCKS5 server"
command="${BIN_DIR}/${BIN_NAME}"
command_args="serve --no-tui --config /etc/next-socks5/config.toml"
command_background=true
pidfile="/run/next-socks5.pid"
output_log="/var/log/next-socks5.log"
error_log="/var/log/next-socks5.log"

depend() {
    need net
}

start_pre() {
    # Create the runtime dir for the admin/attach Unix socket. checkpath is
    # openrc's idempotent mkdir+chmod.
    checkpath -d -m 0710 /run/next-socks5
}
EOF
    $SUDO chmod +x /etc/init.d/next-socks5
    $SUDO rc-update add next-socks5 default
    $SUDO rc-service next-socks5 restart
    STARTED="yes"
    MANAGE_HINT="rc-service next-socks5 status|stop  |  logs: tail -f /var/log/next-socks5.log"
  else
    warn "no supported init system detected (need systemd or OpenRC)"
    print_manual_start
  fi
}

# --- Docker install -----------------------------------------------------------
install_docker() {
  need_cmd docker
  # Support both `docker compose` (v2) and legacy `docker-compose`.
  local compose
  if docker compose version >/dev/null 2>&1; then
    compose="docker compose"
  elif command -v docker-compose >/dev/null 2>&1; then
    compose="docker-compose"
  else
    err "docker compose not found (install Docker Compose v2)"
  fi

  local tag="latest"
  [ "$VERSION" = "latest" ] || tag="${VERSION#v}"   # image tags are unprefixed (e.g. 0.1.0)

  log "preparing deploy dir: ${DEPLOY_DIR}"
  mkdir -p "$DEPLOY_DIR"
  render_config > "${DEPLOY_DIR}/config.toml"

  cat > "${DEPLOY_DIR}/docker-compose.yml" <<EOF
services:
  next-socks5:
    image: ${IMAGE}:${tag}
    container_name: next-socks5
    restart: unless-stopped
    # Host networking so SOCKS5 UDP ASSOCIATE works (the relay advertises a
    # client-reachable BND address). Linux only.
    network_mode: host
    volumes:
      - ./config.toml:/etc/next-socks5/config.toml:ro
    # Writable runtime dir for the admin/attach Unix socket. The image runs as an
    # unprivileged user (uid 65534) that cannot create /run/next-socks5 itself, so
    # without this the admin endpoint is disabled and \`next-socks5 attach\` fails.
    tmpfs:
      - /run/next-socks5
    command: ["--config", "/etc/next-socks5/config.toml"]
EOF

  log "pulling image and starting container"
  ( cd "$DEPLOY_DIR" && $compose pull && $compose up -d )

  # With host networking a port clash (e.g. an existing binary/systemd install on
  # the same port) makes the container crash-loop on bind, yet `up -d` still
  # returns success. Give it a moment, then confirm it actually stayed up — a
  # restarted container means it failed to start. Surface the real error instead
  # of falsely reporting "started".
  local running restarts
  sleep 2
  running="$(docker inspect -f '{{.State.Running}}' next-socks5 2>/dev/null || echo false)"
  restarts="$(docker inspect -f '{{.State.RestartCount}}' next-socks5 2>/dev/null || echo 0)"
  if [ "$running" != "true" ] || [ "${restarts:-0}" -gt 0 ]; then
    warn "container is not healthy — recent logs:"
    docker logs --tail 20 next-socks5 >&2 2>/dev/null || true
    err "container failed to start (often: port ${PORT} already in use by another install — check 'ss -tlnp | grep ${PORT}')"
  fi
  STARTED="yes"
  MANAGE_HINT="cd ${DEPLOY_DIR} && ${compose} logs -f | ${compose} down"
}

# --- Run ----------------------------------------------------------------------
MANAGE_HINT=""
case "$METHOD" in
  binary) install_binary ;;
  docker) install_docker ;;
esac

# --- Summary ------------------------------------------------------------------
host_display="$LISTEN_ADDR"
case "$LISTEN_ADDR" in
  0.0.0.0|::) ip="$(get_public_ip)"; host_display="${ip:-<server-ip>}" ;;
esac

echo ""
if [ "$STARTED" = "yes" ]; then
  log "next-socks5 installed and started ✔"
else
  log "next-socks5 installed — NOT started (see warnings above) ⚠"
fi
echo ""
field "method" "$METHOD"
field "listen" "${LISTEN_ADDR}:${PORT}"
if [ "$AUTH" = "on" ]; then
  field "username" "$USERNAME"
  field "password" "$PASSWORD"
  field "proxy URL" "socks5://${USERNAME}:${PASSWORD}@${host_display}:${PORT}"
  field "test" "curl --socks5 ${USERNAME}:${PASSWORD}@127.0.0.1:${PORT} https://api.ipify.org"
else
  field "auth" "none (open proxy)"
  field "proxy URL" "socks5://${host_display}:${PORT}"
  field "test" "curl --socks5 127.0.0.1:${PORT} https://api.ipify.org"
fi
[ -n "$MANAGE_HINT" ] && field "manage" "$MANAGE_HINT"
if [ "$METHOD" = "docker" ]; then
  field "实时仪表板" "docker exec -it next-socks5 ${BIN_NAME} attach"
else
  field "实时仪表板" "${BIN_DIR}/${BIN_NAME} attach"
fi
