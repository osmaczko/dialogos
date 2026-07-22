# diaλogos

A passive AI chat bot on the [Logos](https://logos.co) chat network, built on
`libchat`. The name is *dia-λogos*: dialogue over Logos. It holds direct (1:1)
conversations and answers each message automatically with a free online LLM:
ChatGPT-style, over end-to-end encrypted Logos chat.

## How it works

1. The operator runs the bot. It prints its address once at startup.
2. The operator shares that address out of band.
3. A person adds the address in a Logos chat client and opens a direct
   conversation. The bot auto-joins (no accept step) and greets.
4. The person chats; the bot replies to each message.
5. If the bot is offline there is no reply, and messages sent while it was down
   are lost.
6. The bot reveals nothing about its host or operator.

## Accepted limitations

This is an experiment on a testnet-stage library. Three limitations are by
design in this version:

- **One run = one address.** The identity (account and delegate) is generated
  fresh each run and never persisted, so every restart yields a new address. The
  MLS group state that backs conversations is held in memory only (the OpenMLS
  provider is memory-backed and never serialized), so it does not survive a
  restart either. The encrypted store at `db_path` persists only bookkeeping
  (conversation metadata and keys), not the MLS group state, so prior
  conversations cannot be resumed after a restart. A durable identity and durable
  conversation storage are separate future work.
- **No offline catch-up.** The transport delivers live traffic only. Messages
  sent while the bot is down are never answered.
- **No identity-layer abuse control.** Auto-join has no allowlist and testnet
  identities are free to mint, so all rate limiting is bot-side and best-effort.
  The global daily budget is the real spend cap, and it is persisted next to the
  store so a crash loop cannot reset it. Per-conversation state is bounded and
  evicted least-recently-active, so a flood of identities cannot grow the bot's
  own memory without limit; the MLS group state libchat keeps per conversation
  is not bot-evictable, so that memory remains unbounded upstream.

## Configuration

Copy `dialogos.example.toml` to `dialogos.toml` and edit it. Everything
non-secret lives there; see the file for the full annotated reference. The two
secrets come from the environment, never the file:

| Variable          | Holds                              |
| ----------------- | ---------------------------------- |
| `DIALOGOS_DB_KEY` | key for the encrypted on-disk store |
| `LLM_API_KEY`     | API key for the LLM provider        |

Under systemd these can instead be passed as credentials (`db_key`, `llm_api_key`
files under `$CREDENTIALS_DIRECTORY`); the bot reads the environment first and
falls back to the credential files. See `deploy/dialogos.service`.

The system prompt is a separate file (`system-prompt.txt`); its path is
resolved relative to the config file. It contains nothing about the host or
operator, and the LLM is given no tools.

### The LLM backend

The bot talks to any OpenAI-compatible `/chat/completions` endpoint, so the
provider is config, not code: swapping a free model for a paid one, or one
vendor for another, is an edit to `[llm]`. Free options include OpenRouter
(`:free` models), Google AI Studio (Gemini free tier), and Groq; a local ollama
works too. Free lists and their limits change, so pick a current model from the
provider rather than trusting a hard-coded id. See the example config.

## Build and run

The build pulls in the native `liblogosdelivery` library (a Nim/nwaku node)
transitively, and the runtime glibc must match the one it was built against: a
plain `cargo build` can link and then fail to start with a
`GLIBC_ABI_DT_X86_64_PLT` error. The flake pins both by following libchat's own
nixpkgs, so nix is the supported path:

```sh
export DIALOGOS_DB_KEY=...        # any strong secret
export LLM_API_KEY=...            # your provider key
nix develop --command cargo run -- --config ./dialogos.toml
# or build the binary / the container image:
nix build .#dialogos
nix build .#image
```

`make run`, `make build`, and `make image` wrap these. Without nix, supply the
native library yourself and build in an environment whose glibc matches it:

```sh
export LOGOS_DELIVERY_LIB_DIR="<path to a prebuilt liblogosdelivery>/lib"
export DIALOGOS_DB_KEY=... LLM_API_KEY=...
cargo run --release -- --config ./dialogos.toml   # a cold build also needs protoc on PATH
```

## Deployment (process isolation)

`deploy/dialogos.service` is a hardened systemd unit: a dedicated unprivileged
`DynamicUser`, `ProtectSystem=strict`, a locked-down syscall filter, all
capabilities dropped, and writes confined to its state directory. It contains
the blast radius of a fault in the in-process native node that parses untrusted
traffic, and denies host access. A hardened container (non-root, read-only
rootfs with a writable volume for the db, `--cap-drop=ALL`,
`--security-opt=no-new-privileges`) is an equivalent alternative. `nix build
.#image` produces that image (the native library and a matching glibc come with
it), and CI publishes it to `ghcr.io/osmaczko/dialogos` on version tags; that
image is the supported deployment artifact.

This is process-level isolation, not network-level anonymity: the p2p node
exposes the host IP to testnet peers, and user messages egress to the LLM
provider. Both are consequences of the chosen posture, not mitigated here.

## Operating

- **Logs**: `journalctl -u dialogos`. `RUST_LOG` controls verbosity (`info` by
  default; `RUST_LOG=dialogos=debug` for the per-conversation detail). Every ten
  minutes the bot logs a `stats` line (events, replies, denies, drops, LLM
  failures, retries, evictions, active conversations, budget spent today).
- **Budget tripped**: a `Daily limit reached` reply to peers and the daily
  counters at the cap in the stats line mean the global budget is spent; replies
  resume at the next UTC midnight. Raise `[limits]` if this trips too early, and
  set a hard spend cap at the provider as a backstop. The budget persists in
  `<db_path>.budget.json`, so a restart does not reset it.
- **Rotating a secret** (`LLM_API_KEY` / `DIALOGOS_DB_KEY`): update the
  environment or the credential file and restart. **A restart mints a new address
  and loses all prior conversations** (see Accepted limitations), so treat it as a
  disruptive action.
- **Watchdog kill**: if the event loop wedges, systemd logs a watchdog timeout in
  `systemctl status dialogos` and restarts the service (again, on a new address).

## Layout

```
src/
  lib.rs      crate root: module declarations and crate-level docs
  main.rs     startup: load config, open the client, print the address, run
  config.rs   TOML + environment/credential secrets, with fail-fast validation
  llm.rs      LlmBackend trait, retry, and the OpenAI-compatible backend
  limits.rs   per-conversation sliding window + persisted global daily budget
  convos.rs   bounded two-tier per-conversation store
  bot.rs      worker-pool event loop, per-conversation state, the reply path
tests/
  event_handling.rs        greeting / reply / ordering / pending policy, deterministic
  direct_conversation.rs   end-to-end over the in-process transport
  worker_pool.rs           the run() loop: concurrency and clean shutdown
```

## Testing

```sh
make test        # nix develop --command cargo test --all-targets
make check       # pins + lint + test + cargo-deny, the same set CI runs
```

The unit tests (config parsing, the rate limiter, the LLM request/response
mapping) live in-crate; the integration tests (a peer talking to the bot over
the in-process transport with a fake backend) live under `tests/`. Compiling the
crate pulls the native `logos-chat` dependency, so the tests need the nix
toolchain (or an equivalent native-library and glibc setup); they run in CI on
every push.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or
  <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
