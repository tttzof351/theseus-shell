#!/usr/bin/env bash
set -euo pipefail

REPO="tttzof351/theseus-shell"
BIN_NAME="theseus"
INSTALL_DIR="${THESEUS_INSTALL_DIR:-"${HOME}/.local/bin"}"
VERSION="${THESEUS_VERSION:-latest}"
TMPDIR_TO_CLEAN=""

cleanup() {
  if [[ -n "${TMPDIR_TO_CLEAN}" ]]; then
    rm -rf "${TMPDIR_TO_CLEAN}"
  fi
}

need_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "error: required command not found: $1" >&2
    exit 1
  fi
}

detect_platform() {
  case "$(uname -s)" in
    Darwin) echo "macos" ;;
    Linux) echo "linux" ;;
    *)
      echo "error: unsupported operating system: $(uname -s)" >&2
      exit 1
      ;;
  esac
}

detect_arch() {
  case "$(uname -m)" in
    x86_64 | amd64) echo "amd64" ;;
    arm64 | aarch64) echo "arm64" ;;
    *)
      echo "error: unsupported CPU architecture: $(uname -m)" >&2
      exit 1
      ;;
  esac
}

download_url() {
  local asset="$1"

  if [[ "${VERSION}" == "latest" ]]; then
    echo "https://github.com/${REPO}/releases/latest/download/${asset}"
    return
  fi

  local tag="${VERSION}"
  if [[ "${tag}" != v* ]]; then
    tag="v${tag}"
  fi

  echo "https://github.com/${REPO}/releases/download/${tag}/${asset}"
}

verify_checksum() {
  local archive="$1"
  local checksum_file="$2"

  if command -v shasum >/dev/null 2>&1; then
    (cd "$(dirname "${archive}")" && shasum -a 256 -c "$(basename "${checksum_file}")")
    return
  fi

  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$(dirname "${archive}")" && sha256sum -c "$(basename "${checksum_file}")")
    return
  fi

  echo "warning: shasum/sha256sum not found, skipping checksum verification" >&2
}

ensure_path_hint() {
  case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) return 0 ;;
  esac

  echo
  echo "The install directory is not in PATH: ${INSTALL_DIR}"

  local shell_name
  shell_name="$(basename "${SHELL:-}")"
  if [[ "${shell_name}" == "zsh" ]]; then
    echo "Add it with: echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> ~/.zshrc"
    echo "Then run: source ~/.zshrc"
  elif [[ "${shell_name}" == "bash" ]]; then
    echo "Add it with: echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> ~/.bashrc"
    echo "Then run: source ~/.bashrc"
  else
    echo "Add this to your shell profile: export PATH=\"${INSTALL_DIR}:\$PATH\""
  fi

  return 1
}

print_next_steps() {
  local install_path="$1"

  echo
  echo "Try it now:"
  echo "  ${install_path}"

  if [[ ":${PATH}:" == *":${INSTALL_DIR}:"* ]]; then
    echo
    echo "Or, because ${INSTALL_DIR} is already in PATH:"
    echo "  ${BIN_NAME}"
  fi
}

main() {
  need_cmd curl
  need_cmd tar
  need_cmd uname

  local platform arch asset archive_url checksum_url tmpdir archive checksum_file extracted_bin install_path
  platform="$(detect_platform)"
  arch="$(detect_arch)"
  asset="${BIN_NAME}-${platform}-${arch}.tar.gz"
  archive_url="$(download_url "${asset}")"
  checksum_url="${archive_url}.sha256"
  tmpdir="$(mktemp -d)"
  TMPDIR_TO_CLEAN="${tmpdir}"
  archive="${tmpdir}/${asset}"
  checksum_file="${archive}.sha256"
  extracted_bin="${tmpdir}/${BIN_NAME}-${platform}-${arch}/${BIN_NAME}"
  install_path="${INSTALL_DIR}/${BIN_NAME}"

  trap cleanup EXIT

  echo "Downloading ${asset} from ${archive_url}"
  curl -fsSL "${archive_url}" -o "${archive}"

  if curl -fsSL "${checksum_url}" -o "${checksum_file}"; then
    verify_checksum "${archive}" "${checksum_file}"
  else
    echo "warning: checksum file not found, skipping checksum verification" >&2
  fi

  tar -xzf "${archive}" -C "${tmpdir}"

  if [[ ! -f "${extracted_bin}" ]]; then
    echo "error: binary not found in archive: ${extracted_bin}" >&2
    exit 1
  fi

  mkdir -p "${INSTALL_DIR}"
  cp "${extracted_bin}" "${install_path}"
  chmod +x "${install_path}"

  if [[ "${platform}" == "macos" ]]; then
    # Local workaround for unsigned release binaries: remove quarantine/provenance
    # metadata and apply an ad-hoc signature to the final installed path.
    if command -v xattr >/dev/null 2>&1; then
      xattr -cr "${install_path}" 2>/dev/null || true
    fi

    if command -v codesign >/dev/null 2>&1; then
      codesign --force --deep --sign - "${install_path}" >/dev/null
    fi
  fi

  echo
  echo "Installed ${BIN_NAME} to: ${install_path}"
  ensure_path_hint || true
  print_next_steps "${install_path}"
}

main "$@"
