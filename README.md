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
  The global daily budget is the real spend cap.

## Configuration

Copy `dialogos.example.toml` to `dialogos.toml` and edit it. Everything
non-secret lives there; see the file for the full annotated reference. The two
secrets come from the environment, never the file:

| Variable          | Holds                              |
| ----------------- | ---------------------------------- |
| `DIALOGOS_DB_KEY` | key for the encrypted on-disk store |
| `LLM_API_KEY`     | API key for the LLM provider        |

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

The bot depends on `logos-chat` by git rev over SSH, so you need SSH access to
the libchat repository. Two facts make the build non-obvious:

- The transitive `logos-delivery-rust` build script needs the native
  `liblogosdelivery` library (a Nim/nwaku node). Point `LOGOS_DELIVERY_LIB_DIR`
  at a prebuilt copy, or the script falls back to running
  `nix build .#logos-delivery` itself (a heavy build).
- The **runtime** glibc must match the native library's. A plain `cargo build`
  can link and then fail to start with a `GLIBC_ABI_DT_X86_64_PLT` error; the
  fix is to build and run inside libchat's `nix develop` so both use the same
  glibc. A cold build also needs `protoc` on `PATH`.

With a libchat checkout that already has the native library built:

```sh
export LOGOS_DELIVERY_LIB_DIR="$(nix build --no-link --print-out-paths <libchat>#logos-delivery)/lib"
export DIALOGOS_DB_KEY=...        # any strong secret
export LLM_API_KEY=...            # your provider key
cargo run --release -- --config ./dialogos.toml
```

If startup hits the glibc symbol error, run the same command inside libchat's
`nix develop`. For a repo others can build without a hand-set env var, add a
`flake.nix` that takes libchat as an input and wires the native library and a
matching glibc for both build and run. This is a follow-up, not needed to run it
here.

## Deployment (process isolation)

`deploy/dialogos.service` is a hardened systemd unit: a dedicated unprivileged
`DynamicUser`, `ProtectSystem=strict`, a locked-down syscall filter, all
capabilities dropped, and writes confined to its state directory. It contains
the blast radius of a fault in the in-process native node that parses untrusted
traffic, and denies host access. A hardened container (non-root, read-only
rootfs with a writable volume for the db, `--cap-drop=ALL`,
`--security-opt=no-new-privileges`) is an equivalent alternative; build the
image via libchat's nix flake so it carries the native library and a matching
glibc.

This is process-level isolation, not network-level anonymity: the p2p node
exposes the host IP to testnet peers, and user messages egress to the LLM
provider. Both are consequences of the chosen posture, not mitigated here.

## Layout

```
src/
  main.rs     startup: load config, open the client, print the address, run
  config.rs   TOML + environment secrets, with fail-fast validation
  llm.rs      LlmBackend trait + the OpenAI-compatible backend
  limits.rs   per-conversation sliding window + global daily budget
  bot.rs      event loop, per-conversation state, the reply path
tests/
  event_handling.rs        greeting / reply / ignore_groups policy, deterministic
  direct_conversation.rs   end-to-end over the in-process transport
```

## Testing

```sh
cargo test
```

The unit tests (config parsing, the rate limiter, the LLM request/response
mapping) and the integration test (a peer talking to the bot over the
in-process transport with a fake backend) all live in-crate. Compiling the
crate pulls the native `logos-chat` dependency, so `cargo test` needs the same
native-library and glibc setup as a build. Run it under the nix/CI path, not on
a box without the native library.

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
