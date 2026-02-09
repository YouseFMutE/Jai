#!/usr/bin/env bash
set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_NAME="aegis-edge-relay"
BIN_PATH="${PROJECT_DIR}/target/release/${BIN_NAME}"
SERVICE_NAME="${BIN_NAME}.service"
SERVICE_FILE="/etc/systemd/system/${SERVICE_NAME}"
ENV_DIR="/etc/${BIN_NAME}"
ENV_FILE="${ENV_DIR}/${BIN_NAME}.env"
DEFAULT_EXIT_LISTEN="0.0.0.0:8443"
DEFAULT_BRIDGE_HEALTH_LISTEN="127.0.0.1:19090"
DEFAULT_EXIT_HEALTH_LISTEN="127.0.0.1:19091"

if [[ "${EUID}" -eq 0 ]]; then
  SUDO=""
else
  SUDO="sudo"
fi

detect_package_manager() {
  if command -v apt-get >/dev/null 2>&1; then
    echo "apt"
  elif command -v dnf >/dev/null 2>&1; then
    echo "dnf"
  elif command -v yum >/dev/null 2>&1; then
    echo "yum"
  elif command -v pacman >/dev/null 2>&1; then
    echo "pacman"
  else
    echo "unsupported"
  fi
}

install_dependencies() {
  local pm
  pm="$(detect_package_manager)"
  case "${pm}" in
    apt)
      ${SUDO} apt-get update
      ${SUDO} env DEBIAN_FRONTEND=noninteractive apt-get install -y \
        build-essential whiptail pkg-config libssl-dev curl ca-certificates
      ;;
    dnf)
      ${SUDO} dnf install -y \
        gcc gcc-c++ make newt pkgconf-pkg-config openssl-devel curl ca-certificates
      ;;
    yum)
      ${SUDO} yum install -y \
        gcc gcc-c++ make newt pkgconfig openssl-devel curl ca-certificates
      ;;
    pacman)
      ${SUDO} pacman -Sy --noconfirm \
        base-devel libnewt pkgconf openssl curl ca-certificates
      ;;
    *)
      echo "Unsupported Linux distribution. Install dependencies manually."
      exit 1
      ;;
  esac
}

ensure_rust() {
  if ! command -v cargo >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  fi
  # shellcheck disable=SC1090
  if [[ -f "${HOME}/.cargo/env" ]]; then
    source "${HOME}/.cargo/env"
  fi
  export PATH="${HOME}/.cargo/bin:${PATH}"
}

build_release() {
  cd "${PROJECT_DIR}"
  cargo build --release
}

validate_port() {
  local value="$1"
  [[ "${value}" =~ ^[0-9]+$ ]] || return 1
  ((value >= 1 && value <= 65535))
}

generate_auth_secret() {
  if command -v openssl >/dev/null 2>&1; then
    openssl rand -hex 32
  else
    tr -dc 'A-Za-z0-9' </dev/urandom | head -c 48
    printf '\n'
  fi
}

collect_inputs() {
  ROLE=$(
    whiptail --title "Aegis Edge Relay" --menu "Current Node Role" 15 72 2 \
      "IRAN_BRIDGE" "Iran Bridge" \
      "FOREIGN_EXIT" "Foreign Exit" \
      3>&1 1>&2 2>&3
  )

  if [[ "${ROLE}" == "IRAN_BRIDGE" ]]; then
    CLEAN_ANYCAST_IP=$(
      whiptail --inputbox "Clean Anycast IP (IPv4/IPv6)" 10 72 "1.1.1.1" \
        3>&1 1>&2 2>&3
    )
    if [[ -z "${CLEAN_ANYCAST_IP}" ]]; then
      echo "Anycast IP is required for bridge mode."
      exit 1
    fi
  else
    CLEAN_ANYCAST_IP=""
  fi

  CUSTOM_DOMAIN=$(
    whiptail --inputbox "Custom Domain/Host (.fun domain)" 10 72 "edge.example.fun" \
      3>&1 1>&2 2>&3
  )
  if [[ -z "${CUSTOM_DOMAIN}" ]]; then
    echo "Custom domain/host is required."
    exit 1
  fi

  KEY_MODE=$(
    whiptail --title "Aegis Edge Relay" --menu "Auth Secret Key Mode" 15 72 2 \
      "MANUAL" "Manual Entry" \
      "RANDOM" "Generate Random" \
      3>&1 1>&2 2>&3
  )

  if [[ "${KEY_MODE}" == "RANDOM" ]]; then
    AUTH_SECRET_KEY="$(generate_auth_secret)"
    whiptail --title "Generated Auth Secret Key" --msgbox \
      "Save this key and use the same value on the other server:\n\n${AUTH_SECRET_KEY}" \
      14 78
  else
    AUTH_SECRET_KEY=$(
      whiptail --passwordbox "Auth Secret Key" 10 72 \
        3>&1 1>&2 2>&3
    )
    if [[ -z "${AUTH_SECRET_KEY}" ]]; then
      echo "Auth secret key is required."
      exit 1
    fi
  fi

  TARGET_PORT=$(
    whiptail --inputbox "Target Port (listener for bridge, forwarder for exit)" 10 72 "8080" \
      3>&1 1>&2 2>&3
  )
  if ! validate_port "${TARGET_PORT}"; then
    echo "Target port must be a valid TCP port (1-65535)."
    exit 1
  fi
}

write_environment_file() {
  ${SUDO} mkdir -p "${ENV_DIR}"
  ${SUDO} chmod 700 "${ENV_DIR}"
  {
    printf 'AUTH_SECRET_KEY="%s"\n' "$(printf '%s' "${AUTH_SECRET_KEY}" | sed 's/"/\\"/g')"
  } | ${SUDO} tee "${ENV_FILE}" >/dev/null
  ${SUDO} chmod 600 "${ENV_FILE}"
}

write_service_file() {
  local exec_args
  if [[ "${ROLE}" == "IRAN_BRIDGE" ]]; then
    exec_args="bridge --listen 0.0.0.0:${TARGET_PORT} --edge-addr ${CLEAN_ANYCAST_IP}:443 --host ${CUSTOM_DOMAIN} --sni ${CUSTOM_DOMAIN} --path /relay --health-listen ${DEFAULT_BRIDGE_HEALTH_LISTEN}"
  else
    exec_args="destination --listen ${DEFAULT_EXIT_LISTEN} --forward 127.0.0.1:${TARGET_PORT} --health-listen ${DEFAULT_EXIT_HEALTH_LISTEN}"
  fi

  cat <<EOF | ${SUDO} tee "${SERVICE_FILE}" >/dev/null
[Unit]
Description=Aegis Edge Relay Service
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
EnvironmentFile=${ENV_FILE}
WorkingDirectory=${PROJECT_DIR}
ExecStart=${BIN_PATH} ${exec_args}
Restart=always
RestartSec=2
LimitNOFILE=1048576
NoNewPrivileges=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF
}

enable_service() {
  ${SUDO} systemctl daemon-reload
  ${SUDO} systemctl enable --now "${SERVICE_NAME}"
}

post_deploy_checks() {
  local health_addr
  if [[ "${ROLE}" == "IRAN_BRIDGE" ]]; then
    health_addr="${DEFAULT_BRIDGE_HEALTH_LISTEN}"
  else
    health_addr="${DEFAULT_EXIT_HEALTH_LISTEN}"
  fi

  echo "== Service status =="
  ${SUDO} systemctl --no-pager --full status "${SERVICE_NAME}" | sed -n '1,25p' || true
  echo
  echo "== ExecStart =="
  ${SUDO} systemctl cat "${SERVICE_NAME}" | grep ExecStart || true
  echo
  echo "== Health check (${health_addr}) =="
  curl -fsS "http://${health_addr}/healthz" || true
  echo
}

main() {
  install_dependencies
  ensure_rust
  build_release
  collect_inputs
  write_environment_file
  write_service_file
  enable_service
  post_deploy_checks

  whiptail --title "Aegis Edge Relay" --msgbox \
    "Deployment completed.\nService: ${SERVICE_NAME}\nBinary: ${BIN_PATH}\nEnv: ${ENV_FILE}" \
    12 80
}

main "$@"
