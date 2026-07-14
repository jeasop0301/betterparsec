#!/usr/bin/env bash
set -euo pipefail

force=0
if [[ "${1:-}" == "--force" ]]; then
  force=1
elif [[ $# -gt 0 ]]; then
  echo "Usage: $0 [--force]" >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
vendor_root="$repo_root/vendor"
target="$vendor_root/moonlight-common-rust"
common_c="$target/moonlight-common-sys/moonlight-common-c"
rust_patch="$repo_root/patches/moonlight-common-rust.patch"
tls_patch="$repo_root/patches/moonlight-common-rust-tls-pinning.patch"
common_c_patch="$repo_root/patches/moonlight-common-c.patch"

rust_url="https://github.com/MrCreativ3001/moonlight-common-rust.git"
rust_revision="df9f1e3003fb4834dbb17a4bd4d3cf25d2fea3d9"
common_c_revision="62687809b1f7410c3db4be2527503a54ae408d70"

patch_applied() {
  local repository="$1"
  local patch="$2"
  git -C "$repository" apply --reverse --check --ignore-space-change --ignore-whitespace "$patch" >/dev/null 2>&1
}

apply_patches() {
  git -C "$target" apply --check --ignore-space-change --ignore-whitespace "$rust_patch"
  git -C "$target" apply --ignore-space-change --ignore-whitespace --whitespace=nowarn "$rust_patch"
  git -C "$target" apply --check --ignore-space-change --ignore-whitespace "$tls_patch"
  git -C "$target" apply --ignore-space-change --ignore-whitespace --whitespace=nowarn "$tls_patch"
  git -C "$common_c" apply --check --ignore-space-change --ignore-whitespace "$common_c_patch"
  git -C "$common_c" apply --ignore-space-change --ignore-whitespace --whitespace=nowarn "$common_c_patch"
}

clean_pinned_base() {
  [[ "$(git -C "$target" rev-parse HEAD 2>/dev/null || true)" == "$rust_revision" ]] || return 1
  [[ "$(git -C "$common_c" rev-parse HEAD 2>/dev/null || true)" == "$common_c_revision" ]] || return 1
  git -C "$target" diff --quiet --ignore-submodules=dirty || return 1
  git -C "$common_c" diff --quiet || return 1
}

ready() {
  [[ -d "$target/.git" ]] || return 1
  [[ "$(git -C "$target" rev-parse HEAD 2>/dev/null || true)" == "$rust_revision" ]] || return 1
  [[ -d "$common_c/.git" || -f "$common_c/.git" ]] || return 1
  [[ "$(git -C "$common_c" rev-parse HEAD 2>/dev/null || true)" == "$common_c_revision" ]] || return 1
  # rust_patch and tls_patch both modify src/http/client/tokio_hyper.rs, so an
  # independent `git apply --reverse --check` of the lower patch fails once the
  # other is layered on top (shifted context). apply_patches already forward
  # --check's each patch before applying; here we detect the applied state by a
  # stable marker each patch introduces, which composes across layered patches.
  grep -q "fn change_bitrate" "$target/src/stream/c/mod.rs" || return 1
  grep -q "PinnedServerVerifier" "$target/src/http/client/tokio_hyper.rs" || return 1
  patch_applied "$common_c" "$common_c_patch" || return 1
}

if ready; then
  echo "Pinned BetterParsec dependency is ready: $target"
  exit 0
fi

if [[ -e "$target" ]]; then
  if clean_pinned_base; then
    apply_patches
    ready || { echo "Existing pinned dependency was patched, but verification failed." >&2; exit 1; }
    echo "Completed patches in existing pinned dependency: $target"
    exit 0
  fi
  if [[ "$force" -ne 1 ]]; then
    echo "Dependency directory exists but is incomplete or differs from the pinned revisions: $target" >&2
    echo "Re-run with --force to recreate only this generated vendor directory." >&2
    exit 1
  fi
  rm -rf -- "$target"
fi

mkdir -p "$vendor_root"
git clone --no-checkout "$rust_url" "$target"
git -C "$target" config core.autocrlf false
git -C "$target" checkout --detach "$rust_revision"
git -C "$target" submodule update --init --recursive

git -C "$common_c" config core.autocrlf false
git -C "$common_c" reset --hard "$common_c_revision"

apply_patches

if ! ready; then
  echo "Dependency bootstrap completed, but revision or patch verification failed." >&2
  exit 1
fi

echo "Bootstrapped pinned BetterParsec dependency: $target"
echo "  moonlight-common-rust: $rust_revision + patches/moonlight-common-rust.patch"
echo "                         + patches/moonlight-common-rust-tls-pinning.patch"
echo "  moonlight-common-c:    $common_c_revision + patches/moonlight-common-c.patch"
