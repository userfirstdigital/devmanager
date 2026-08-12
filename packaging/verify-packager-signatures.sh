#!/usr/bin/env bash
# Cryptographically verify cargo-packager updater signatures with DEVMANAGER_UPDATE_PUBKEY.
set -euo pipefail

artifact_dir="${1:?artifact directory required}"
pubkey="${DEVMANAGER_UPDATE_PUBKEY:?DEVMANAGER_UPDATE_PUBKEY is required}"
minisign_bin="${MINISIGN_BIN:-minisign}"

if [[ ! -d "${artifact_dir}" ]]; then
  echo "Artifact directory missing: ${artifact_dir}" >&2
  exit 1
fi
if ! command -v "${minisign_bin}" >/dev/null 2>&1; then
  echo "minisign not found (${minisign_bin})" >&2
  exit 1
fi

work="$(mktemp -d)"
cleanup() { rm -rf "${work}"; }
trap cleanup EXIT

pub_path="${work}/minisign.pub"
if printf '%s' "${pubkey}" | base64 --decode >"${pub_path}" 2>/dev/null; then
  :
else
  printf '%s\n' "${pubkey}" >"${pub_path}"
fi

mapfile -t sig_files < <(find "${artifact_dir}" -type f -name '*.sig' | sort)
if [[ "${#sig_files[@]}" -lt 1 ]]; then
  echo "No .sig files under ${artifact_dir}" >&2
  exit 1
fi

verified=0
for sig_file in "${sig_files[@]}"; do
  artifact="${sig_file%.sig}"
  if [[ ! -f "${artifact}" ]]; then
    echo "Signature has no sibling artifact: ${sig_file}" >&2
    exit 1
  fi
  decoded="${work}/$(basename "${sig_file}").minisig"
  if base64 --decode <"${sig_file}" >"${decoded}" 2>/dev/null; then
    :
  else
    cp "${sig_file}" "${decoded}"
  fi
  "${minisign_bin}" -V -p "${pub_path}" -m "${artifact}" -x "${decoded}"
  echo "Verified signature for ${artifact}"
  verified=$((verified + 1))
done

echo "Verified ${verified} cargo-packager updater signature(s) under ${artifact_dir}"
