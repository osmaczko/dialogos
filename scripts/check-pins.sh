#!/usr/bin/env bash
# Fail if the libchat pin drifts between Cargo.toml, flake.nix, and flake.lock.
# The rev lives in three places and a bump must touch all of them in one commit;
# this guards that invariant in CI. flake.lock is optional so the check also
# passes before the flake has been locked on a nix host.
set -euo pipefail

cd "$(dirname "$0")/.."

fail() { echo "check-pins: $*" >&2; exit 1; }

# All libchat git deps in Cargo.toml must share one rev.
mapfile -t cargo_revs < <(grep -oE 'libchat\.git", rev = "[0-9a-f]{40}"' Cargo.toml | grep -oE '[0-9a-f]{40}' | sort -u)
[ "${#cargo_revs[@]}" -eq 1 ] || fail "expected one libchat rev in Cargo.toml, found ${#cargo_revs[@]}: ${cargo_revs[*]:-none}"
cargo_rev="${cargo_revs[0]}"

# flake.nix libchat input rev.
flake_rev="$(grep -oE 'libchat\.url = "github:logos-messaging/libchat\?rev=[0-9a-f]{40}"' flake.nix | grep -oE '[0-9a-f]{40}' || true)"
[ -n "$flake_rev" ] || fail "could not read the libchat input rev from flake.nix"
[ "$flake_rev" = "$cargo_rev" ] || fail "flake.nix rev ($flake_rev) != Cargo.toml rev ($cargo_rev)"

# flake.lock locked rev, if the flake has been locked.
if [ -f flake.lock ]; then
  lock_rev="$(jq -r '.nodes.libchat.locked.rev // empty' flake.lock)"
  [ -n "$lock_rev" ] || fail "flake.lock has no locked libchat rev"
  [ "$lock_rev" = "$cargo_rev" ] || fail "flake.lock rev ($lock_rev) != Cargo.toml rev ($cargo_rev)"
else
  echo "check-pins: flake.lock absent (not yet locked); skipping that comparison" >&2
fi

echo "check-pins: libchat pinned consistently at $cargo_rev"
