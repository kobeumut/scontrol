#!/usr/bin/env bash
# scontrol installer — Ubuntu/Debian only.
# Run as root: curl -fsSL <release-url>/install.sh | sudo bash
# Optional components are deliberately opt-in; unselected packages are never
# installed and their corresponding Rust code is omitted from panel-agent.
set -Eeuo pipefail
IFS=$'\n\t'

readonly PROJECT_NAME="scontrol"
readonly INSTALL_ROOT="/opt/scontrol"
readonly CONFIG_ROOT="/etc/scontrol"
readonly DATA_ROOT="/var/lib/scontrol"
readonly RUNTIME_ROOT="/run/scontrol"
readonly PANEL_USER="scontrol"
readonly PANEL_GROUP="scontrol"
readonly REPO_URL_DEFAULT="https://github.com/REPLACE_ME/scontrol.git"

log() { printf '\033[1;34m[scontrol]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[scontrol]\033[0m %s\n' "$*" >&2; exit 1; }
require_root() { [[ "${EUID}" -eq 0 ]] || die "run this installer as root"; }

check_os() {
  [[ -r /etc/os-release ]] || die "cannot identify operating system"
  # shellcheck disable=SC1091
  . /etc/os-release
  case "${ID:-}" in
    ubuntu|debian) ;;
    *) die "unsupported OS: ${ID:-unknown}; Ubuntu and Debian are supported" ;;
  esac
}

ask_yes_no() {
  local prompt="$1" answer
  while true; do
    read -r -p "$prompt [y/N] " answer || true
    case "${answer,,}" in
      y|yes) return 0 ;;
      ""|n|no) return 1 ;;
      *) printf 'Please answer y or n.\n' ;;
    esac
  done
}

install_base_packages() {
  log "Installing base packages"
  export DEBIAN_FRONTEND=noninteractive
  apt-get update
  apt-get install -y --no-install-recommends \
    ca-certificates curl git gnupg lsb-release \
    build-essential pkg-config libssl-dev nodejs npm \
    postgresql postgresql-contrib \
    nginx certbot python3-certbot-nginx \
    ufw cron
}

install_rust() {
  if command -v cargo >/dev/null 2>&1; then
    return
  fi
  log "Installing Rust toolchain"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  # shellcheck disable=SC1091
  . /root/.cargo/env
}

install_gh() {
  if command -v gh >/dev/null 2>&1; then
    return
  fi
  log "Installing GitHub CLI"
  install -d -m 0755 /etc/apt/keyrings
  curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
    -o /etc/apt/keyrings/githubcli-archive-keyring.gpg
  chmod go+r /etc/apt/keyrings/githubcli-archive-keyring.gpg
  echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
    > /etc/apt/sources.list.d/github-cli.list
  apt-get update
  apt-get install -y --no-install-recommends gh
}

force_github_login() {
  log "GitHub login is required for automatic pull requests."
  # Deliberately interactive: no token is accepted or persisted by this script.
  gh auth status >/dev/null 2>&1 || gh auth login --web --git-protocol https
  gh auth status >/dev/null 2>&1 || die "GitHub login did not complete"
}

clone_or_update_source() {
  local repo_url="$1"
  if [[ -d "${INSTALL_ROOT}/.git" ]]; then
    log "Updating existing source checkout"
    git -C "$INSTALL_ROOT" fetch --prune origin
    git -C "$INSTALL_ROOT" pull --ff-only
  else
    log "Cloning source repository"
    rm -rf "$INSTALL_ROOT"
    git clone --depth 1 "$repo_url" "$INSTALL_ROOT"
  fi
}

create_accounts_and_dirs() {
  getent group "$PANEL_GROUP" >/dev/null || groupadd --system "$PANEL_GROUP"
  id -u "$PANEL_USER" >/dev/null 2>&1 || \
    useradd --system --gid "$PANEL_GROUP" --home-dir "$DATA_ROOT" --create-home --shell /usr/sbin/nologin "$PANEL_USER"

  install -d -o root -g "$PANEL_GROUP" -m 0750 "$CONFIG_ROOT" "$DATA_ROOT" "$RUNTIME_ROOT"
  install -d -o "$PANEL_USER" -g "$PANEL_GROUP" -m 0750 "$DATA_ROOT"/uploads
  install -d -o root -g root -m 0755 /srv/scontrol/sites
}

# Pinned application builders (opt-in, Docker only). Each download is
# checked against the SHA-256 recorded here before anything is installed;
# bump a version only together with its checksums.
readonly NIXPACKS_VERSION="1.41.0"
readonly RAILPACK_VERSION="0.43.0"
readonly PACK_VERSION="0.40.10"

builder_sha256() {
  case "$1" in
    nixpacks-amd64) echo 0f55de7874507b9cf7502113120bd96f2ab6979f78d10eaf2eb2ade9207b3af6 ;;
    nixpacks-arm64) echo 912bd02dd2bb6f9c3a9ed965fe8a68b4aa318dc7a2546e2eca6f2806a894ba39 ;;
    railpack-amd64) echo 248a8f60f36854aabed6124113631078a802de3461660855fc4de9c26065c1fb ;;
    railpack-arm64) echo 732ece14eb4f4676a43ebba19431f954edf7b130acb76159ef0f2ff86ac42ac9 ;;
    pack-amd64) echo 687c3616e21824a449b56d4f989613ca68c921c6cce670b4de4459b025233806 ;;
    pack-arm64) echo 9a294abb0576fd95e19a009b3f9d5f5aa9fecdc2d7da7fa05e7e1e94f5085d77 ;;
    *) return 1 ;;
  esac
}

# Downloads <url>, verifies it, and installs the <binary> it contains.
install_pinned_binary() {
  local name="$1" url="$2" binary="$3" arch="$4" sha tmp
  sha="$(builder_sha256 "${name}-${arch}")" || die "no pinned checksum for ${name} on ${arch}"
  tmp="$(mktemp -d)"
  curl --proto '=https' --tlsv1.2 -fsSL -o "${tmp}/archive.tgz" "$url"
  echo "${sha}  ${tmp}/archive.tgz" | sha256sum -c --quiet - \
    || { rm -rf "$tmp"; die "checksum mismatch for ${name}; refusing to install it"; }
  tar -xzf "${tmp}/archive.tgz" -C "$tmp" "$binary"
  install -m 0755 -o root -g root "${tmp}/${binary}" "/usr/local/bin/${binary}"
  rm -rf "$tmp"
}

install_builders() {
  local arch
  arch="$(dpkg --print-architecture)"
  case "$arch" in
    amd64|arm64) ;;
    *) die "application builders are available for amd64 and arm64 only (found ${arch})" ;;
  esac
  log "Installing application builders (Nixpacks ${NIXPACKS_VERSION}, Railpack ${RAILPACK_VERSION}, pack ${PACK_VERSION})"
  # Railpack builds through BuildKit (`docker buildx`).
  apt-get install -y --no-install-recommends docker-buildx \
    || apt-get install -y --no-install-recommends docker-buildx-plugin \
    || true
  docker buildx version >/dev/null 2>&1 \
    || log "warning: docker buildx is missing; Railpack builds will fail until it is installed"
  local nix_arch="x86_64" rail_arch="x86_64" pack_suffix=""
  if [[ "$arch" == arm64 ]]; then nix_arch="aarch64"; rail_arch="arm64"; pack_suffix="-arm64"; fi
  install_pinned_binary nixpacks \
    "https://github.com/railwayapp/nixpacks/releases/download/v${NIXPACKS_VERSION}/nixpacks-v${NIXPACKS_VERSION}-${nix_arch}-unknown-linux-musl.tar.gz" \
    nixpacks "$arch"
  install_pinned_binary railpack \
    "https://github.com/railwayapp/railpack/releases/download/v${RAILPACK_VERSION}/railpack-v${RAILPACK_VERSION}-${rail_arch}-unknown-linux-musl.tar.gz" \
    railpack "$arch"
  install_pinned_binary pack \
    "https://github.com/buildpacks/pack/releases/download/v${PACK_VERSION}/pack-v${PACK_VERSION}-linux${pack_suffix}.tgz" \
    pack "$arch"
}

random_cookie_key() {
  # 32 raw bytes encoded once; never print it or add it to Git. Also used
  # for PANEL_SECRET_KEY, which encrypts stored notification credentials.
  # An existing env file is left alone: an install that predates
  # PANEL_SECRET_KEY keys those secrets from PANEL_COOKIE_KEY, and adding a
  # new key later would make them unreadable.
  openssl rand -base64 32
}

# The `[remote]` listener of an agent-only install: mutual TLS on the
# chosen port, reachable only from the panel's address (UFW).
readonly REMOTE_PORT_DEFAULT=7843

write_remote_section() {
  local port="$1"
  cat >> "${CONFIG_ROOT}/agent.toml" <<EOF

[remote]
listen = "0.0.0.0:${port}"
ca_cert = "${CONFIG_ROOT}/remote/ca.pem"
cert = "${CONFIG_ROOT}/remote/agent.pem"
key = "${CONFIG_ROOT}/remote/agent.key"
EOF
}

# Accepts an IPv4/IPv6 address or CIDR, nothing else (it goes into a UFW rule).
valid_panel_source() {
  [[ "$1" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}(/[0-9]{1,2})?$ ]] \
    || [[ "$1" =~ ^[0-9A-Fa-f:]+(/[0-9]{1,3})?$ && "$1" == *:* ]]
}

# Hostnames or IP addresses the panel will connect to (the certificate's name).
valid_agent_host() {
  [[ "$1" =~ ^[A-Za-z0-9]([A-Za-z0-9.-]{0,251}[A-Za-z0-9])?$ ]] || valid_panel_source "$1"
}

# Agent-only install: a server managed by a scontrol panel elsewhere. No
# PostgreSQL, panel-api, frontend, or GitHub automation here.
main_agent() {
  local repo_url="$1"
  local docker_enabled=false mail_enabled=false antivirus_enabled=false builders_enabled=false
  local agent_host panel_source port
  read -r -p "Hostname or IP address the panel connects to (goes into this agent's certificate): " agent_host || true
  valid_agent_host "$agent_host" || die "invalid host: ${agent_host}"
  read -r -p "Panel address allowed to connect (IP or CIDR): " panel_source || true
  valid_panel_source "$panel_source" || die "invalid address: ${panel_source}"
  read -r -p "Listen port [${REMOTE_PORT_DEFAULT}]: " port || true
  port="${port:-$REMOTE_PORT_DEFAULT}"
  [[ "$port" =~ ^[0-9]+$ ]] && (( port >= 1 && port <= 65535 )) || die "invalid port: ${port}"
  ask_yes_no "Install optional Docker support?" && docker_enabled=true
  if [[ "$docker_enabled" == true ]]; then
    ask_yes_no "Install application builders (Nixpacks, Railpack, Buildpacks) for apps without a Dockerfile?" \
      && builders_enabled=true
  fi
  ask_yes_no "Install optional mail support (Postfix/Dovecot/DKIM)?" && mail_enabled=true
  ask_yes_no "Install optional antivirus support (ClamAV)?" && antivirus_enabled=true

  log "Installing base packages (agent only)"
  export DEBIAN_FRONTEND=noninteractive
  apt-get update
  apt-get install -y --no-install-recommends \
    ca-certificates curl git gnupg build-essential pkg-config libssl-dev \
    nginx certbot python3-certbot-nginx ufw cron
  install_rust
  clone_or_update_source "$repo_url"
  create_accounts_and_dirs
  install_optional_packages "$docker_enabled" "$mail_enabled" "$antivirus_enabled"
  [[ "$builders_enabled" == true ]] && install_builders
  write_configuration "$docker_enabled" "$mail_enabled" "$antivirus_enabled"
  write_remote_section "$port"

  local features=""
  [[ "$docker_enabled" == true ]] && features="${features:+$features,}docker"
  [[ "$builders_enabled" == true ]] && features="${features:+$features,}builders"
  [[ "$mail_enabled" == true ]] && features="${features:+$features,}mail"
  [[ "$antivirus_enabled" == true ]] && features="${features:+$features,}antivirus"
  log "Building panel-agent"
  # shellcheck disable=SC1091
  [[ -f /root/.cargo/env ]] && . /root/.cargo/env
  pushd "${INSTALL_ROOT}/panel" >/dev/null
  local -a cargo_args=(build --release -p panel-agent)
  [[ -n "$features" ]] && cargo_args+=(--features "$features")
  SQLX_OFFLINE=true cargo "${cargo_args[@]}"
  install -m 0755 target/release/panel-agent /usr/local/bin/panel-agent
  popd >/dev/null

  # The private key is created here and never leaves this server.
  install -d -o root -g root -m 0700 "${CONFIG_ROOT}/remote"
  local csr
  if [[ -f "${CONFIG_ROOT}/remote/agent.key" ]]; then
    log "Keeping the existing agent key (rotate with: panel-agent remote-init --host ${agent_host} --force)"
    csr="$(cat "${CONFIG_ROOT}/remote/agent.csr" 2>/dev/null || true)"
  else
    csr="$(panel-agent remote-init --host "$agent_host" --dir "${CONFIG_ROOT}/remote")"
  fi

  ufw allow from "$panel_source" to any port "$port" proto tcp comment 'scontrol panel to agent (mTLS)'
  install -m 0644 "${INSTALL_ROOT}/systemd/panel-agent.service" /etc/systemd/system/panel-agent.service
  systemctl daemon-reload
  systemctl enable --now panel-agent.service

  log "Agent installed. In the panel, open Servers, Add server, Mutual TLS:"
  log "  address ${agent_host}:${port}, and paste this certificate signing request:"
  printf '%s\n' "$csr"
  log "Save the returned certificate as ${CONFIG_ROOT}/remote/agent.pem and the CA as ${CONFIG_ROOT}/remote/ca.pem."
  log "The agent starts serving the panel as soon as both files exist; no restart is needed."
}

write_configuration() {
  local docker_enabled="$1" mail_enabled="$2" antivirus_enabled="$3"
  local env_file="${CONFIG_ROOT}/panel-api.env"

  if [[ ! -f "$env_file" ]]; then
    umask 077
    cat > "$env_file" <<EOF
DATABASE_URL=postgresql:///scontrol?user=scontrol
PANEL_API_BIND=127.0.0.1:8080
PANEL_AGENT_SOCKET=${RUNTIME_ROOT}/agent.sock
PANEL_COOKIE_KEY=$(random_cookie_key)
PANEL_SECRET_KEY=$(random_cookie_key)
PANEL_SESSION_TTL_HOURS=168
EOF
    chown root:"$PANEL_GROUP" "$env_file"
    chmod 0640 "$env_file"
  fi

  cat > "${CONFIG_ROOT}/agent.toml" <<EOF
socket_path = "${RUNTIME_ROOT}/agent.sock"
sites_root = "/srv/scontrol/sites"
nginx_sites_available = "/etc/nginx/sites-available"
nginx_sites_enabled = "/etc/nginx/sites-enabled"
systemd_unit_dir = "/etc/systemd/system"
docker_projects_root = "/var/lib/scontrol/docker"
acme_webroot = "/var/lib/scontrol/acme"
letsencrypt_live_dir = "/etc/letsencrypt/live"
deploy_timeout_secs = 1800
deployment_log_max_bytes = 5242880
deployment_logs_retained = 20
rollback_releases_retained = 5
static_sites_root = "/srv/scontrol/static"
nginx_stream_dir = "/etc/nginx/scontrol-stream"
nginx_stats_log = "/var/log/nginx/scontrol-access.json.log"
railpack_frontend = "ghcr.io/railwayapp/railpack-frontend:v${RAILPACK_VERSION}"
buildpacks_builder = "paketobuildpacks/builder-jammy-base"

[runtime_features]
docker = ${docker_enabled}
mail = ${mail_enabled}
antivirus = ${antivirus_enabled}
EOF
  chown root:root "${CONFIG_ROOT}/agent.toml"
  chmod 0644 "${CONFIG_ROOT}/agent.toml"
}

# Compose domain vhosts answer ACME HTTP-01 challenges from this webroot
# (certbot --webroot). The deploy hook reloads Nginx after each renewal so
# renewed certificates are picked up; it is a fixed command, not user input.
configure_acme_webroot() {
  install -d -o root -g root -m 0755 /var/lib/scontrol/acme
  install -d -o root -g root -m 0755 /etc/letsencrypt/renewal-hooks/deploy
  cat > /etc/letsencrypt/renewal-hooks/deploy/scontrol-reload-nginx <<'HOOK'
#!/bin/sh
exec systemctl reload nginx
HOOK
  chmod 0755 /etc/letsencrypt/renewal-hooks/deploy/scontrol-reload-nginx
}

# Database services are reachable from outside only through generated Nginx
# `stream` servers (never a Docker port on 0.0.0.0, which would bypass UFW).
# The stream module is a separate package; the top-level `stream {}` block
# goes into modules-enabled because Debian's nginx.conf includes that
# directory in the main context, after the module that defines `stream`.
configure_nginx_stream() {
  apt-get install -y --no-install-recommends libnginx-mod-stream
  install -d -o root -g root -m 0755 /etc/nginx/scontrol-stream
  cat > /etc/nginx/modules-enabled/99-scontrol-stream.conf <<'NGINX'
# Managed by scontrol: database services opened for outside access.
stream {
    include /etc/nginx/scontrol-stream/*.conf;
}
NGINX
  chmod 0644 /etc/nginx/modules-enabled/99-scontrol-stream.conf
  nginx -t
}

install_optional_packages() {
  local docker_enabled="$1" mail_enabled="$2" antivirus_enabled="$3"
  if [[ "$docker_enabled" == true ]]; then
    log "Installing optional Docker support"
    # openssh-client provides ssh and ssh-keygen for private Git deploy keys;
    # git only recommends it, and recommends are skipped here.
    apt-get install -y --no-install-recommends docker.io openssh-client
    # Deployments use the Compose v2 CLI plugin (`docker compose`). Ubuntu
    # ships it as docker-compose-v2; on Debian it comes from Docker's repo.
    apt-get install -y --no-install-recommends docker-compose-v2 \
      || apt-get install -y --no-install-recommends docker-compose-plugin \
      || true
    docker compose version >/dev/null 2>&1 \
      || die "Docker Compose v2 ('docker compose') is required; install docker-compose-plugin from Docker's apt repository and re-run"
    configure_acme_webroot
    configure_nginx_stream
    # Static sites: release directories the Nginx worker reads directly.
    install -d -o root -g root -m 0755 /srv/scontrol/static
  fi
  if [[ "$mail_enabled" == true ]]; then
    log "Installing optional mail support"
    apt-get install -y --no-install-recommends postfix dovecot-core dovecot-imapd opendkim
  fi
  if [[ "$antivirus_enabled" == true ]]; then
    log "Installing optional antivirus support"
    apt-get install -y --no-install-recommends clamav clamav-daemon
  fi
}

build_frontend() {
  log "Building panel web frontend"
  pushd "${INSTALL_ROOT}/panel/frontend" >/dev/null
  npm install --no-audit --no-fund
  npm run build
  install -d -o root -g root -m 0755 /var/www/scontrol
  rm -rf /var/www/scontrol/*
  cp -a dist/. /var/www/scontrol/
  popd >/dev/null
}

configure_nginx() {
  install -m 0644 "${INSTALL_ROOT}/install/nginx-scontrol.conf" /etc/nginx/sites-available/scontrol.conf
  ln -sfn /etc/nginx/sites-available/scontrol.conf /etc/nginx/sites-enabled/scontrol.conf
  rm -f /etc/nginx/sites-enabled/default
  # JSON access log for request statistics, and its rotation.
  install -m 0644 "${INSTALL_ROOT}/install/nginx-scontrol-log.conf" /etc/nginx/conf.d/scontrol-log.conf
  install -m 0644 "${INSTALL_ROOT}/install/logrotate-scontrol-nginx" /etc/logrotate.d/scontrol-nginx
  nginx -t
  systemctl reload nginx
}

build_and_install() {
  local features="$1"
  log "Building release binaries"
  # shellcheck disable=SC1091
  [[ -f /root/.cargo/env ]] && . /root/.cargo/env
  pushd "${INSTALL_ROOT}/panel" >/dev/null
  # SQLx query metadata is committed, so no database URL is needed during compilation.
  local -a cargo_args=(build --release --workspace)
  [[ -n "$features" ]] && cargo_args+=(--features "$features")
  SQLX_OFFLINE=true cargo "${cargo_args[@]}"
  install -m 0755 target/release/panel-agent /usr/local/bin/panel-agent
  install -m 0755 target/release/panel-api /usr/local/bin/panel-api
  install -m 0755 target/release/panel-cli /usr/local/bin/panel-cli
  popd >/dev/null
}

install_services() {
  install -m 0644 "${INSTALL_ROOT}/systemd/panel-agent.service" /etc/systemd/system/panel-agent.service
  install -m 0644 "${INSTALL_ROOT}/systemd/panel-api.service" /etc/systemd/system/panel-api.service
  install -m 0644 "${INSTALL_ROOT}/systemd/scontrol-git-sync.path" /etc/systemd/system/scontrol-git-sync.path
  install -m 0644 "${INSTALL_ROOT}/systemd/scontrol-git-sync.service" /etc/systemd/system/scontrol-git-sync.service
  # Template for SSH-forwarded remote agents (`ssh_socket` servers); enabled
  # per server by the operator, see the unit file.
  install -m 0644 "${INSTALL_ROOT}/systemd/scontrol-agent-tunnel@.service" /etc/systemd/system/scontrol-agent-tunnel@.service
  install -d -o root -g "$PANEL_GROUP" -m 0750 "${CONFIG_ROOT}/tunnels"
  install -m 0755 "${INSTALL_ROOT}/install/auto-pr.sh" /usr/local/lib/scontrol-auto-pr

  systemctl daemon-reload
  systemctl enable --now panel-agent.service panel-api.service scontrol-git-sync.path
}

setup_database() {
  # Local peer authentication: no DB password is written to disk.
  sudo -u postgres psql -tc "SELECT 1 FROM pg_roles WHERE rolname = 'scontrol'" | grep -q 1 || \
    sudo -u postgres createuser --no-createdb --no-createrole --no-superuser scontrol
  sudo -u postgres psql -tc "SELECT 1 FROM pg_database WHERE datname = 'scontrol'" | grep -q 1 || \
    sudo -u postgres createdb --owner=scontrol scontrol
}

main() {
  require_root
  check_os

  local repo_url="${SCONTROL_REPO_URL:-$REPO_URL_DEFAULT}"
  [[ "$repo_url" != *"REPLACE_ME"* ]] || die "set SCONTROL_REPO_URL to this project's Git repository URL"

  # SCONTROL_ROLE=agent skips the question (unattended agent installs).
  if [[ "${SCONTROL_ROLE:-}" == "agent" ]] \
    || { [[ -z "${SCONTROL_ROLE:-}" ]] && ask_yes_no "Install only a remote agent (this server is managed by a scontrol panel on another server)?"; }; then
    main_agent "$repo_url"
    return
  fi

  local docker_enabled=false mail_enabled=false antivirus_enabled=false
  local builders_enabled=false
  ask_yes_no "Install optional Docker support?" && docker_enabled=true
  if [[ "$docker_enabled" == true ]]; then
    ask_yes_no "Install application builders (Nixpacks, Railpack, Buildpacks) for apps without a Dockerfile?" \
      && builders_enabled=true
  fi
  ask_yes_no "Install optional mail support (Postfix/Dovecot/DKIM)?" && mail_enabled=true
  ask_yes_no "Install optional antivirus support (ClamAV)?" && antivirus_enabled=true

  install_base_packages
  install_rust
  install_gh
  force_github_login
  clone_or_update_source "$repo_url"
  create_accounts_and_dirs
  setup_database
  install_optional_packages "$docker_enabled" "$mail_enabled" "$antivirus_enabled"
  [[ "$builders_enabled" == true ]] && install_builders
  write_configuration "$docker_enabled" "$mail_enabled" "$antivirus_enabled"

  local features=""
  [[ "$docker_enabled" == true ]] && features="${features:+$features,}panel-agent/docker"
  [[ "$builders_enabled" == true ]] && features="${features:+$features,}panel-agent/builders"
  [[ "$mail_enabled" == true ]] && features="${features:+$features,}panel-agent/mail"
  [[ "$antivirus_enabled" == true ]] && features="${features:+$features,}panel-agent/antivirus"
  build_and_install "$features"
  build_frontend
  configure_nginx
  install_services

  local host_ip
  host_ip="$(hostname -I | awk '{print $1}')"
  log "Installation complete. Open http://${host_ip}/ (API health: http://${host_ip}/health/)"
  log "Create the first administrator with: sudo -u scontrol panel-cli user create --username admin --email you@example.com --password '<12+ chars>' --admin"
}

main "$@"
