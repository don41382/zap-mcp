# zap-mcp

**An unofficial MCP server for WhatsApp messages.** Lets Claude and Codex read (and, if you allow it, send) your WhatsApp chats.

> [!CAUTION]
> **Unofficial software. Use entirely at your own risk.**
> This project is not affiliated with, authorized, or endorsed by WhatsApp or
> Meta. It uses an unofficial, reverse-engineered client, which WhatsApp's
> Terms of Service do not allow. **WhatsApp may temporarily or permanently ban
> your phone number** for using it. Your message archive contains other
> people's personal data, and you are responsible for it. The software comes
> with no warranty and no liability. Read the full [DISCLAIMER](DISCLAIMER.md)
> before you install. The installer asks you to accept it.

An MCP server that gives Claude (or any MCP client) access to your WhatsApp
messages. Pure Rust, built on [whatsapp-rust](https://github.com/oxidezap/whatsapp-rust)
(a Rust implementation of the WhatsApp Web multi-device protocol, inspired by
whatsmeow/Baileys) and [rmcp](https://crates.io/crates/rmcp), the official
Rust MCP SDK.

It links as an additional **linked device** on your WhatsApp account (like
WhatsApp Web), saves the history your phone sends plus every new message to a
local SQLite archive, and serves that archive as MCP tools. For personal use
with your own account only: no bulk, automated, or commercial messaging.

## Tools

| Tool | What it does |
|---|---|
| `whatsapp_status` | Link and connection state, archive size and date range |
| `list_chats` | Chats ordered by last activity, optional name filter, last-message preview |
| `get_messages` | One chat's messages by name, phone number, or JID, with optional `after`/`before` window |
| `search_messages` | Substring search across all chats or one chat |
| `recent_messages` | Everything since a given time (default: last 24h) |
| `send_message` | Send a text message. **Off unless you allow it during setup** (`allow_send` in `settings.json`) |

Media is listed by type and caption (`[image] menu`), not downloaded. Edits
update the stored text; "delete for everyone" removes the stored text and keeps
a `[deleted]` marker.

## Install

The program is its own installer. When **you** start it (double-click it, or
run it in a terminal), it runs setup. Afterwards it runs as one small
background service that keeps WhatsApp connected and that all Claude and
Codex windows share.

1. Quit Claude and Codex, then run one line in a terminal:

   **macOS** (Terminal):
   ```sh
   curl -fsSL https://github.com/don41382/zap-mcp/releases/latest/download/install.sh | sh
   ```

   **Windows** (PowerShell):
   ```powershell
   irm https://github.com/don41382/zap-mcp/releases/latest/download/install.ps1 | iex
   ```

   The script downloads the right build for your computer from the latest
   release, checks it against the release's `SHA256SUMS.txt`, and starts the
   guided setup. Because it's not a browser download, macOS Gatekeeper and
   Windows SmartScreen don't block it.

2. Setup walks you through five steps:
   1. **Disclaimer.** You must read and accept it to continue.
   2. **Install.** Copies the program to `~/.local/bin` (macOS) or
      `%LOCALAPPDATA%\Programs\zap-mcp` (Windows).
   3. **Options.** Whether Claude may send messages (default: no), and whether
      the background service starts when you log in (recommended).
   4. **Connect apps.** Adds zap-mcp to Claude Code, Claude Desktop and Codex
      (CLI, IDE extension and the Codex app) if they're installed. Your
      existing Claude Desktop config is kept, with a backup next to it.
   5. **Link WhatsApp.** You choose either:
      - **QR code:** WhatsApp → Settings → Linked Devices → Link a Device, then scan.
      - **Phone number:** setup shows an 8-character code, which you enter under
        Link a Device → "Link with phone number instead".

      It then downloads your message history and finishes on its own when
      no new messages have arrived for a while. How much history you get is up
      to your phone; usually it's recent months.

   Finally it starts the background service and checks that it answers.

3. Start Claude or Codex and ask something like *"What did I miss on WhatsApp today?"*

**Updating or uninstalling:** run the install line again (or start the
installed `zap-mcp`). It detects the existing installation and offers:
- **Update**: replaces the program and restarts the background service,
  keeping your settings, app registrations and WhatsApp link. Claude and
  Codex reconnect by themselves.
- **Change settings**: runs the full setup again.
- **Uninstall**: stops the service and removes it from start-at-login, Claude
  and Codex. It asks whether to also delete your message archive and WhatsApp
  session (default: keep) and whether to remove the program.

**ChatGPT** (the chat, not Codex) can only use MCP servers that are reachable
on the internet, not ones on your computer. Making your WhatsApp archive
reachable from the internet isn't something zap-mcp supports, on purpose.
Codex, including Codex inside the ChatGPT app, works as described above.

Run `zap-mcp status` at any time to see whether the service is running and WhatsApp is connected.

<details>
<summary><b>Manual download instead of the install script</b></summary>

Download the archive for your system from the
[Releases](https://github.com/don41382/zap-mcp/releases/latest) page
(`zap-mcp-aarch64-apple-darwin.tar.gz` for Apple Silicon Macs,
`zap-mcp-x86_64-apple-darwin.tar.gz` for Intel Macs,
`zap-mcp-x86_64-pc-windows-msvc.zip` for Windows), unpack it and start
`zap-mcp`. Releases are not code-signed, so for browser downloads macOS may say
the app "cannot be opened because the developer cannot be verified"
(right-click it → Open) and Windows SmartScreen may show "Windows protected
your PC" ("More info" → "Run anyway").

</details>

<details>
<summary><b>Commands / manual setup / other MCP clients</b></summary>

```sh
zap-mcp setup       # the guided installer (same as double-click)
zap-mcp status      # is the service running, is WhatsApp linked and connected
zap-mcp login [--qr | --phone +4915112345678]   # only (re-)link WhatsApp
zap-mcp daemon      # run the background service in the foreground (logs to stderr)
zap-mcp serve       # stdio MCP relay to the service, for clients that launch commands
zap-mcp uninstall [--purge]   # remove service, start-at-login, app registrations (--purge: and data)
```

The service listens on `http://127.0.0.1:<port>/mcp`. The port and the access
token are in `~/.zap-mcp/settings.json`, along with `allow_send`; restart the
service after you change them.

Claude Code connects over HTTP:

```sh
claude mcp add -s user --transport http whatsapp http://127.0.0.1:<port>/mcp \
  -H "Authorization: Bearer <token>"
```

Claude Desktop (`~/Library/Application Support/Claude/claude_desktop_config.json`
or `%APPDATA%\Claude\claude_desktop_config.json`) can only launch commands, so
it uses the relay, which starts the service if it isn't running:

```json
{
  "mcpServers": {
    "whatsapp": { "command": "/Users/<you>/.local/bin/zap-mcp", "args": ["serve"] }
  }
}
```

Codex (`~/.codex/config.toml`, shared by the CLI, IDE extension and Codex app):

```sh
codex mcp add whatsapp -- ~/.local/bin/zap-mcp serve
```

Any other MCP client: use the HTTP URL with the token, or run `zap-mcp serve` over stdio.

</details>

<details>
<summary><b>Build from source</b></summary>

Needs Rust ≥ 1.94 (stable) and a C compiler (Xcode Command Line Tools on
macOS, Visual Studio Build Tools on Windows).

```sh
cargo build --release
./target/release/zap-mcp     # runs setup
```

The `simd` feature of whatsapp-rust needs nightly Rust, so it's turned off in `Cargo.toml`.

</details>

## How it runs

```
Claude Code ──────── HTTP + token ────────┐
                                          ├──> zap-mcp daemon ──> WhatsApp
Claude Desktop, Codex ──stdio──> zap-mcp serve (relay) ┘       │
                                                          messages.db
```

- **One background service** (`zap-mcp daemon`) holds the WhatsApp connection
  and serves MCP. It starts at login (macOS: LaunchAgent
  `~/Library/LaunchAgents/dev.zap-mcp.daemon.plist`; Windows: a per-user Run
  entry) and keeps saving new messages even while Claude is closed. That also
  prevents WhatsApp from unlinking the device after about 14 days offline.
- **Local only, token-protected.** It listens on `127.0.0.1` only, needs the
  random token from `settings.json` on every request, accepts only loopback
  `Host` headers, and rejects requests from web pages (any `Origin` header).
  Other programs and websites can't reach your messages.
- **One connection.** WhatsApp allows one connection per linked device. A
  session lock makes sure only one service process ever connects.

## Data

Everything lives in `~/.zap-mcp/` (Windows: `%USERPROFILE%\.zap-mcp`). Data from
versions before the rename (`~/.whatsapp-rust-mcp`) is moved there automatically.
You can override the location with `--data-dir` or `ZAP_MCP_DATA_DIR`:

- `session.db` holds the linked device's encryption keys. Anyone with this
  file can read and send your messages, so treat it like a password.
- `messages.db` is the message archive (plain SQLite, unencrypted).
- `settings.json` holds the port, access token, and `allow_send`.
- `daemon.log` is the service log; `daemon.pid` and `session.lock` are runtime files.
- `terms-accepted.txt` records when you accepted the disclaimer, and for which version.

On macOS the folder is restricted to your user (`0700`). Nothing is sent
anywhere except to WhatsApp's servers and to the MCP clients you connect. When
Claude reads your messages, that content goes to Anthropic as part of your
conversation.

If the computer is off, messages are delivered when the service next
connects. If WhatsApp ever unlinks the device, run `zap-mcp login` (or setup) again.

### Uninstall

1. Start zap-mcp and choose **Uninstall**, or run `zap-mcp uninstall` (add
   `--purge` to also delete the message archive and WhatsApp session). Either
   way it stops the service and removes zap-mcp from start-at-login, Claude
   Code, Claude Desktop and Codex.
2. On your phone, remove the device under WhatsApp → Linked Devices.
3. If you kept them: delete the program and the `.zap-mcp` data folder.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
# installs/removes the real LaunchAgent / Run entry against a throwaway data dir:
ZAP_TEST_DIR=/tmp/zap-test ZAP_TEST_EXE=$PWD/target/debug/zap-mcp cargo test -- --ignored autostart_roundtrip
```

- `src/main.rs`: CLI (setup or relay depending on how it was started), data dir, session lock
- `src/setup.rs`: guided installer (disclaimer, install, options, Claude registration, uninstall)
- `src/daemon.rs`: background service (HTTP MCP, token check, health), start/stop, start-at-login
- `src/proxy.rs`: stdio relay for clients that launch commands (`serve`)
- `src/link.rs`: linking by QR or pairing code, initial history download
- `src/ingest.rs`: WhatsApp events (live messages, history sync, contact sync, group names) → archive
- `src/store.rs`: SQLite archive, LID→phone-number folding, queries
- `src/server.rs`: MCP tools

WhatsApp increasingly addresses people by an opaque *LID* instead of their
phone number. The store records LID↔number mappings from history sync, contact
sync, and message envelopes, and merges each person's chats under their
phone-number JID.

### Releases

`.github/workflows/build.yml` runs clippy and tests and builds release
binaries for macOS (Apple Silicon and Intel) and Windows on every push and
pull request. Builds are downloadable as workflow artifacts. Pushing a tag
like `v0.1.0` also publishes a GitHub Release with the archives and a
`SHA256SUMS.txt`:

```sh
git tag v0.1.0 && git push origin v0.1.0
```

To cross-check the Windows build from a Mac: `cargo xwin clippy --target x86_64-pc-windows-msvc`.
This needs LLVM's `clang-cl` first on your `PATH` (e.g. `/opt/homebrew/opt/llvm/bin`).

## License

[MIT](LICENSE), provided "as is" without warranty. See also the [DISCLAIMER](DISCLAIMER.md).
Release archives include `THIRD_PARTY_LICENSES.html` with the licences of all
bundled dependencies, generated by [cargo-about](https://github.com/EmbarkStudios/cargo-about)
from `about.toml`. To generate it locally, run `cargo about generate about.hbs -o THIRD_PARTY_LICENSES.html`.
"WhatsApp" is a trademark of WhatsApp LLC, used here only to describe compatibility.
