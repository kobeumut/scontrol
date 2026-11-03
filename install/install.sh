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

random_cookie_key() {
  # 32 raw bytes encoded once; never print it or add it to Git.
  openssl rand -base64 32
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

[runtime_features]
docker = ${docker_enabled}
mail = ${mail_enabled}
antivirus = ${antivirus_enabled}
EOF
  chown root:root "${CONFIG_ROOT}/agent.toml"
  chmod 0644 "${CONFIG_ROOT}/agent.toml"
}

install_optional_packages() {
  local docker_enabled="$1" mail_enabled="$2" antivirus_enabled="$3"
  if [[ "$docker_enabled" == true ]]; then
    log "Installing optional Docker support"
    apt-get install -y --no-install-recommends docker.io
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

  local docker_enabled=false mail_enabled=false antivirus_enabled=false
  ask_yes_no "Install optional Docker support?" && docker_enabled=true
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
  write_configuration "$docker_enabled" "$mail_enabled" "$antivirus_enabled"

  local features=""
  [[ "$docker_enabled" == true ]] && features="${features:+$features,}panel-agent/docker"
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
