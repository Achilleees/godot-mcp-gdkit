# gdkit — Godot MCP toolkit

gdkit lets an MCP client run Godot projects, check GDScript, run tests, import assets,
export builds, and look up the installed engine's API. It runs as a native server over
stdio or authenticated Streamable HTTP and includes a Claude Code plugin manifest.

## Requirements

- Windows 10 or newer.
- Godot 4.6 or newer. Tested with Godot 4.7.1.
- An MCP client, such as Claude Code.
- Rust and PowerShell 7 to build from source.

## Build and install

From the repository root, run:

```powershell
./scripts/check.ps1 -GodotBin 'C:/path/to/godot_console.exe' -Install
```

This checks formatting, runs Clippy and the tests with Godot, builds the release
executable, and installs it as `bin/godot-mcp.exe`. Stop any running instance of that
executable before replacing it. Omit `-Install` to run the checks and build without
installing. `GODOT_BIN` or the saved engine path can replace the explicit path argument.

Load the plugin into Claude Code from a Godot project directory:

```text
claude --plugin-dir <path-to-gdkit>
```

For another MCP client, configure a stdio server whose command is the absolute path
to `bin/godot-mcp.exe`. Set `GODOT_PROJECT` to the game directory, or launch the server
with that directory as its working directory. The server uses stdout for MCP messages
and stderr for diagnostic logs.

## Tools

| Tool | Function |
| --- | --- |
| `ping` | Check connectivity and optionally echo a message. |
| `run` | Start the project or a scene, with optional headless mode and extra Godot arguments. |
| `status` | Read process state and recent logs for one or all runs. |
| `stop` | Stop one or all managed runs. |
| `check_script` | Parse-check a GDScript file using Godot's headless checker. |
| `run_tests` | Run an explicit, saved, or detected test command and report its verdict and errors. |
| `reimport` | Import assets and rebuild Godot's global script-class cache. |
| `export` | List export presets or produce a release build, debug build, or resource pack. |
| `docs` | Look up classes, inherited members, or search terms in the installed engine's API. |
| `config` | Inspect resolved settings and save or clear overrides. |

`run_tests` detects gdUnit4, GUT, and supported script harnesses by their runner files.
An explicit command takes precedence over a saved command, which takes precedence over
detection. Commands accept `{godot}` and `{project}` placeholders.
Timeouts cover the command and its output streams. Windows commands run in owned
process jobs, so timeout, cancellation, and shutdown also terminate their descendants.
Captured logs retain up to 4,000 lines and 4 MiB, with cumulative error counts.
Lines over 64 KiB are truncated;
incomplete capture cannot produce a passing verdict. A test command that exits zero
with engine errors is reported as `PASSED (with engine errors)`.

`reimport` reports failure when Godot prints engine errors, even if it exits zero.

`export` reads `export_presets.cfg`. Specify a preset when the project has more than
one, or save a default with `config`. Godot export templates are required for the
chosen platform for executable builds. An export succeeds only when the engine exits
successfully without reported errors and the output is a nonempty file whose metadata
shows it was created or updated. An unchanged previous build cannot satisfy this check.

`docs` generates its reference from the installed engine and caches it by engine
version. Queries can name a class (`Sprite2D`), a member (`Node.queue_free`), or a
substring. Pass `refresh: true` to rebuild the cached reference. Engine changes reload
the index; failed refreshes preserve the previous valid cache and report an error.

The current tool set does not provide screenshots, live scene-tree inspection, or
input injection.

## Configuration

gdkit searches for Godot in this order:

1. `GODOT_BIN`.
2. The engine path saved through `config`.
3. `PATH` and common installation directories.

It searches for a directory containing `project.godot` in this order:

1. `GODOT_PROJECT`.
2. The project directory saved through `config`.
3. `CLAUDE_PROJECT_DIR`.
4. The server's working directory.

The `config` tool can save `godot_bin`, `project_dir`, `test_command`, `export_preset`,
and `export_output`. Empty values clear individual overrides; `clear: true` clears
all saved settings. A saved project directory applies to subsequent server launches;
use `GODOT_PROJECT` to select a different game for one launch.

Settings and the API cache use `GDKIT_DATA_DIR` when set, otherwise
`CLAUDE_PLUGIN_DATA`, then the operating system's per-user data directory.
The engine's import cache and requested export artifacts are written to the game
project or the output locations selected by the client.

## HTTP transport

Stdio is the default. To serve MCP over HTTP, set a bearer token and pass `--http`:

```powershell
$env:GDKIT_HTTP_TOKEN = '<your-bearer-token>'
./bin/godot-mcp.exe --http 127.0.0.1:8642
```

The endpoint is `http://127.0.0.1:8642/mcp`. Clients must send
`Authorization: Bearer <your-bearer-token>` on every request. The server refuses to
start HTTP without a token and returns HTTP 401 for requests without a matching token.
`--token` also accepts a token, but exposes it in the process command line.

`--http` alone binds to `127.0.0.1:8642`. Pass an address or port to change the listener;
`--bind` overrides that address. Binding beyond loopback makes the server reachable
through the selected interface. Use an encrypted connection when carrying the bearer
token over a network.

Access to this server allows clients to launch Godot, execute test commands, and
write import and export output. Connect only clients trusted to perform those actions.

## License

MIT. See [LICENSE](LICENSE).
