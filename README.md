# zap-mcp

**An unofficial MCP server for WhatsApp messages.** Lets Claude read (and, if you allow it, send) your WhatsApp chats.

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
| `send_message` | Send a text message. **Off unless you allow it during setup** (`serve --allow-send`) |

Media is listed by type and caption (`[image] menu`), not downloaded. Edits
update the stored text; "delete for everyone" removes the stored text and keeps
a `[deleted]` marker.

## Install

The program is its own installer. When **you** start it (double-click it, or
run it in a terminal), it runs setup. When **Claude** starts it, it acts as
the MCP server.

1. Download the file for your system from the repository's **Releases** page and unpack it:

   | System | File |
   |---|---|
   | Mac with Apple Silicon (M1 or newer) | `zap-mcp-aarch64-apple-darwin.tar.gz` |
   | Mac with Intel | `zap-mcp-x86_64-apple-darwin.tar.gz` |
   | Windows (64-bit) | `zap-mcp-x86_64-pc-windows-msvc.zip` |

2. Start `zap-mcp` (macOS: double-click it, or run `./zap-mcp`
   in Terminal; Windows: double-click `zap-mcp.exe`). Setup walks you
   through four steps:
   1. **Disclaimer.** You must read and accept it to continue.
   2. **Install.** Copies the program to `~/.local/bin` (macOS) or
      `%LOCALAPPDATA%\Programs\zap-mcp` (Windows).
   3. **Connect to Claude.** Asks whether Claude may send messages, then adds
      the server to Claude Code and/or Claude Desktop if they're installed.
      Your existing Claude Desktop config is kept, with a backup next to it.
   4. **Link WhatsApp.** You choose either:
      - **QR code:** WhatsApp → Settings → Linked Devices → Link a Device, then scan.
      - **Phone number:** setup shows an 8-character code, which you enter under
        Link a Device → "Link with phone number instead".

      It then downloads your message history and finishes on its own when
      no new messages have arrived for a while. How much history you get is up
      to your phone; usually it's recent months.

3. Restart Claude and ask something like *"What did I miss on WhatsApp today?"*

If macOS says the app "cannot be opened because the developer cannot be
verified", right-click it → Open. On Windows, SmartScreen may show "Windows
protected your PC": click "More info" → "Run anyway". This only happens while
releases aren't code-signed.

<details>
<summary><b>Manual setup / other MCP clients</b></summary>

Subcommands, if you'd rather not use the guided setup:

```sh
zap-mcp setup                 # the guided installer (same as double-click)
zap-mcp login [--qr | --phone +4915112345678]   # only link WhatsApp
zap-mcp serve [--allow-send]  # the MCP server on stdio
```

Register the server yourself:

```sh
claude mcp add -s user whatsapp -- ~/.local/bin/zap-mcp serve
```

Claude Desktop (`~/Library/Application Support/Claude/claude_desktop_config.json`
or `%APPDATA%\Claude\claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "whatsapp": { "command": "/Users/<you>/.local/bin/zap-mcp", "args": ["serve"] }
  }
}
```

Any other MCP client: run `zap-mcp serve` over stdio.

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

## Data and multiple instances

Everything lives in `~/.zap-mcp/` (Windows: `%USERPROFILE%\.zap-mcp`). Data from
versions before the rename (`~/.whatsapp-rust-mcp`) is moved there automatically.
You can override the location with `--data-dir` or `ZAP_MCP_DATA_DIR`:

- `session.db` holds the linked device's encryption keys. Anyone with this
  file can read and send your messages, so treat it like a password.
- `messages.db` is the message archive (plain SQLite, unencrypted).
- `session.lock` makes sure only one process connects at a time.
- `terms-accepted.txt` records when you accepted the disclaimer, and for which version.

On macOS the folder is restricted to your user (`0700`). Nothing is sent
anywhere except to WhatsApp's servers and to the MCP client you connect. When
Claude reads your messages, that content goes to Anthropic as part of your
conversation.

WhatsApp allows one connection per linked device. The first server process
takes the lock and connects; any further instances (a second Claude window,
Claude Desktop and Claude Code at once) run **read-only** on the shared
archive with sending disabled. Linking won't start while a server holds the
lock, so quit Claude first if you need to re-link.

Messages that arrive while no server is running are delivered when it next
connects. WhatsApp unlinks companion devices that stay offline for about 14
days; if that happens, run setup again.

### Uninstall

1. On your phone, remove the device under Linked Devices.
2. Run `claude mcp remove whatsapp -s user`, and delete the `whatsapp` entry from Claude Desktop's config.
3. Delete the program and the `.zap-mcp` data folder.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

- `src/main.rs`: CLI, chooses setup or server depending on how the program was started, session lock
- `src/setup.rs`: guided installer (disclaimer, install, Claude registration)
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
