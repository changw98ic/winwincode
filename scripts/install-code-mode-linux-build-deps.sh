#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

source /etc/os-release
case "${VERSION_CODENAME}" in
  noble|bookworm) ;;
  *) printf 'Unsupported Code Mode build distribution: %s\n' "${VERSION_CODENAME}" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  aarch64) compiler_target=aarch64-unknown-linux-gnu; runtime_arch=aarch64 ;;
  x86_64) compiler_target=x86_64-unknown-linux-gnu; runtime_arch=x86_64 ;;
  *) printf 'Unsupported Code Mode build architecture\n' >&2; exit 1 ;;
esac

key_file=$(mktemp)
trap 'rm -f -- "${key_file}"' EXIT
curl -fsS --retry 2 --max-time 60 https://apt.llvm.org/llvm-snapshot.gpg.key -o "${key_file}"
fingerprint=$(gpg --show-keys --with-colons "${key_file}" | awk -F: '$1 == "fpr" { print $10; exit }')
test "${fingerprint}" = 6084F3CF814B57C1CF12EFD515CF4D18AF4F7421
install -m 0644 "${key_file}" /usr/share/keyrings/winwincode-llvm.asc
printf 'deb [signed-by=/usr/share/keyrings/winwincode-llvm.asc] https://apt.llvm.org/%s/ llvm-toolchain-%s-23 main\n' \
  "${VERSION_CODENAME}" "${VERSION_CODENAME}" > /etc/apt/sources.list.d/winwincode-llvm.list
apt-get update -y
DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
  binutils bubblewrap pkg-config libcap-dev libglib2.0-dev \
  clang-23 libclang-23-dev libclang-rt-23-dev lld-23 llvm-23

# Chromium GN uses the target-triple resource layout; Debian packages use
# lib/linux and an architecture suffix. Bind GN to the installed native bytes.
resource_dir=$(/usr/lib/llvm-23/bin/clang -print-resource-dir)
native_builtins="${resource_dir}/lib/linux/libclang_rt.builtins-${runtime_arch}.a"
gn_builtins="${resource_dir}/lib/${compiler_target}/libclang_rt.builtins.a"
test -f "${native_builtins}"
install -d -m 0755 "$(dirname "${gn_builtins}")"
if [[ ! -e "${gn_builtins}" ]]; then
  ln -s "../linux/libclang_rt.builtins-${runtime_arch}.a" "${gn_builtins}"
fi
cmp --silent "${native_builtins}" "${gn_builtins}"
/usr/lib/llvm-23/bin/clang --version
/usr/lib/llvm-23/bin/llvm-ar --version
