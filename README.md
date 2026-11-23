# Bias CLI

`bias` runs every operation in the [Bias API](https://biaspay.com/docs/api) from your terminal. You
sign in through the Bias dashboard, and it completes commands and parameters in zsh, bash and fish.

## Install

```sh
brew install biaspay/tap/bias                     # macOS and Linux
npm install -g @biaspay/cli                       # any platform with Node 18+
curl -fsSL https://biaspay.com/docs/install.sh | sh   # macOS and Linux, into ~/.local/bin
```

Windows builds are on [the releases page](https://github.com/BiasPay/cli/releases), and npm installs
them too. Each archive is listed in the release's `SHA256SUMS` and has a build provenance attestation
(`gh attestation verify <archive> --repo BiasPay/cli`).

## Usage

```sh
bias login                          # opens the dashboard to approve this device
bias customers list --limit 3 --table
bias customers create --name "Jane Doe" --email jane@example.com
bias listen --forward-to http://localhost:3000/webhooks
```

`bias help` lists the commands, and `bias help <command>` shows a command's parameters. Signed in to
a live account, a command that makes a change needs `--live`.

| Variable          | Effect                                                            |
| ----------------- | ----------------------------------------------------------------- |
| `BIAS_API_KEY`    | Authenticates with an API key instead of a sign-in, such as in CI |
| `BIAS_CONFIG_DIR` | Where profiles and tokens are kept (default `~/.config/bias`)     |
| `BIAS_NO_BROWSER` | Set to `1` to print the sign-in link instead of opening a browser |
| `BIAS_OUTPUT`     | Set to `table` to print tables by default                         |

To sign a device out, delete its key under Developers → API keys in the dashboard, or run
`bias logout` on it.

## Development

This repository is published from the Bias monorepo, where the CLI lives in `packages/cli`; changes
made here are overwritten by the next release.

```sh
cargo build --release      # target/release/bias
cargo test
```

`manifest.json` is the command table, with each command's response schema and the columns `--table`
shows by default (`COLUMN_OVERRIDES` in the generator adjusts them). `bun sdk:generate cli` (or
`--all`) regenerates it from `packages/backend/openapi/openapi.json`, and the binary embeds it. CI
fails when it drifts from the spec (`.buildkite/scripts/check-sdk-drift.sh`).

Against the local stack:

```sh
export BIAS_API_URL=https://api.bias.localhost   # the dashboard URL follows: api. → app.
bias login                                       # or --device, --read-only, --profile <name>
```

| File          | What it does                                                                    |
| ------------- | ------------------------------------------------------------------------------- |
| `cli.rs`      | Reads argv into a command, path params, a body and flags; runs it               |
| `params.rs`   | Path notation (`--a.b[0].c=3`) and schema-typed values                          |
| `loose.rs`    | The forgiving JSON reader: unquoted keys, trailing commas, comments, bare words |
| `manifest.rs` | The embedded command table and schema lookups                                   |
| `client.rs`   | Requests: token refresh under a file lock, retries, idempotency keys            |
| `auth.rs`     | `login` (PKCE with a loopback redirect, or a device code), `logout`, `auth …`   |
| `config.rs`   | Profiles in `config.json`; tokens in `credentials.json` (0600)                  |
| `complete.rs` | `bias __complete` and the zsh, bash and fish scripts                            |
| `table.rs`    | `--table` rows and columns, and `--fields` picking                              |
| `help.rs`     | Help text from the manifest                                                     |

`packages/internal-shared/src/cli` is the TypeScript port of the parsing, completion, help and table
rules (`cli.rs`, `params.rs`, `loose.rs`, `manifest.rs`, `complete.rs`, `table.rs`, `help.rs`) that
the dashboard console runs against the same `manifest.json`. A change to those rules here needs the
same change there, with its test.

The server side is `packages/backend/src/ops/oauth` and `src/api/routes/oauth` (token, device and
revoke endpoints), with the consent pages at `/oauth/authorize` and `/device` in the dashboard. Each
sign-in is a grant backed by its own API key, so requests log against it and deleting the key signs
the CLI out.

### Releasing

Bump `version` in `Cargo.toml`. After the next deploy from `main`, the publisher pushes this package
to [BiasPay/cli](https://github.com/BiasPay/cli) with a `v<version>` tag, and its release workflow
builds each platform's binary, publishes the GitHub release, the npm packages (`@biaspay/cli` and one
`@biaspay/cli-<os>-<cpu>` per platform) and the Homebrew formula. The checks fail when anything that
ships in the binary changed since the last release without a version bump
(`.buildkite/scripts/check-cli-version.sh`).
