# Thin wrappers over the nix-backed toolchain. `make check` runs the same checks
# as CI (CI is authoritative; the cargo-deny versions can differ, since CI uses
# the pinned action and this uses nixpkgs' cargo-deny). Every Rust target runs
# inside `nix develop` so it picks up the pinned toolchain and the native
# logos-delivery library.

.PHONY: check lint test build image run deny pins lock fmt

check: pins lint test deny

lint:
	nix develop --command bash -euo pipefail -c "cargo fmt --all --check && cargo clippy --all-targets --all-features -- -D warnings"

test:
	nix develop --command cargo test --all-targets

build:
	nix build .#dialogos --print-build-logs

image:
	nix build .#image --print-build-logs

run:
	nix develop --command cargo run -- --config ./dialogos.toml

fmt:
	nix develop --command cargo fmt --all

deny:
	nix run nixpkgs#cargo-deny -- check advisories bans licenses sources

pins:
	./scripts/check-pins.sh

# One-time bootstrap / after a pin bump: refresh both lockfiles. Requires a nix
# host (the flake.lock cannot be generated without nix).
lock:
	nix develop --command cargo generate-lockfile
	nix flake lock
