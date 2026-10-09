# metroctl

Run and manage a whole React Native project from a single terminal window: start
and drive Metro, boot simulators/emulators, build & run the app on them, forward
Metro's interactive keys (`r`/`d`/`j`…), and watch the JS logs / network / perf —
all at once. Built on top of [simon](https://github.com/hvalec427/simon) for
device management.

```sh
metroctl init     # register the current directory as a project
metroctl          # open the dashboard for the current project
metroctl config   # print the config file path
metroctl logs     # just the React Native log viewer (see docs/logs.md)
metroctl up …     # dashboard set up for this checkout (port, simulator, build)
metroctl down     # stop the `up` session in this checkout
metroctl gc       # clean up after sessions that died (--sims: orphaned simulators)
metroctl mcp      # MCP server for coding agents (see below)
```

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/hvalec427/metroctl/master/install.sh | sh
```

Grab the binary from the [latest release](https://github.com/hvalec427/metroctl/releases/latest), or the bleeding edge with `… | sh -s -- dev`. Re-run the installer to update. Uninstall:

```sh
curl -fsSL https://raw.githubusercontent.com/hvalec427/metroctl/master/uninstall.sh | sh
```

Prefer source? `cargo install --git https://github.com/hvalec427/metroctl`.

> **macOS only.** Booting simulators and streaming device logs rely on macOS
> tooling (`xcrun`, `simctl`, `adb`), via simon.

## Setup

1. From your project root, register it:
   ```sh
   cd ~/dev/my-rn-app
   metroctl init          # adds this project to ~/.config/metroctl/config.json
   ```
   `init` detects your package manager and prints the commands it will run, so for
   a standard project you're already done.
2. (Optional) Open the config to customize — `metroctl config` prints its path:
   ```sh
   $EDITOR "$(metroctl config | head -1)"
   ```
   See the [field reference](#field-reference) below. A minimal entry is just a
   `name` and `root`; everything else has a default.
3. From anywhere inside the project, launch the dashboard:
   ```sh
   metroctl
   ```

## Config

Projects live in `~/.config/metroctl/config.json`, a registry keyed by repo root.
metroctl picks the project whose `root` is a prefix of your current directory,
so you just `cd` into a repo and run `metroctl`.

`metroctl init` scaffolds a minimal entry; everything else falls back to sensible
defaults derived from your package manager (detected from the lockfile). A full
entry looks like:

```jsonc
{
  "projects": [
    {
      "name": "MyApp",
      "root": "/Users/me/dev/myapp",      // matched as a prefix of the cwd
      "packageManager": "yarn",           // optional; npm | yarn | pnpm (auto-detected)
      "metro":   { "command": "yarn start", "port": 8081 },
      "ios":     { "command": "yarn ios", "bundleId": "com.myapp" },
      "android": { "command": "yarn android", "bundleId": "com.myapp" },
      "deeplinks": [                       // optional; the `l` quick-picker
        "myapp://home",
        { "name": "Order 42", "url": "myapp://orders/42" }
      ]
    }
  ]
}
```

Anything omitted is derived:

| Package manager | metro | ios | android |
|---|---|---|---|
| npm | `npm start` | `npm run ios` | `npm run android` |
| yarn | `yarn start` | `yarn ios` | `yarn android` |
| pnpm | `pnpm start` | `pnpm ios` | `pnpm android` |

**Port in one place.** Set `metro.port` and nothing else — metroctl connects its log
feed to that port *and* exports it as `RCT_METRO_PORT` into every command it runs,
so Metro and your builds use the same port. It defaults to `8081`.

**Which device?** Pick one live from the Devices pane (`j`/`k` to select) and press
`⏎` to build & run on it. If it's a sim/emulator that isn't up yet, `⏎` boots it
first — press `⏎` again once it shows `● running` (its id isn't known until then,
and building early targets the wrong device). metroctl targets your pick with
`--udid` (iOS) and `--deviceId` + the `ANDROID_SERIAL` env (Android).

If your build script ends in a `-- …` passthrough, an appended `--udid`/`--deviceId`
would land on the wrong side of the `--` and be ignored. Put a **`{udid}`** (iOS) /
**`{serial}`** (Android) placeholder in `ios.command` / `android.command` instead, so
it lands exactly where you want — metroctl substitutes the selected device:

```json
"ios": { "command": "npx react-native run-ios --udid {udid} --scheme 'MyApp' -- --reset-cache" }
```

Android rarely needs this — the `ANDROID_SERIAL` env pins the device regardless of `--`.

### Field reference

| Field | Required | Default | Purpose |
|---|---|---|---|
| `name` | yes | — | Display name for the project. |
| `root` | yes | — | Absolute repo path; simon picks the project whose `root` is a prefix of your cwd (longest match wins). `rn init` stores the canonical path. |
| `packageManager` | no | auto | `npm` \| `yarn` \| `pnpm`; detected from the lockfile when omitted. Drives the default commands. |
| `metro.command` | no | `<pm> start` | Command to start Metro. |
| `metro.port` | no | `8081` | Metro port. Used for the log feed **and** exported as `RCT_METRO_PORT` to every command — set it only here. |
| `ios.command` | no | `<pm> run ios` / `<pm> ios` | Build & run command for iOS. May contain `{udid}`, replaced with the selected device (put it before any `-- …`). |
| `ios.bundleId` | no | — | App bundle id; when set, `o` delivers the link straight to the app on a **physical** iPhone instead of Safari. |
| `android.command` | no | `<pm> run android` / `<pm> android` | Build & run command for Android. May contain `{serial}`, replaced with the selected device (usually unnecessary — `ANDROID_SERIAL` already pins it). |
| `android.bundleId` | no | — | Application id; when set, `o` routes the link to that package instead of a browser/chooser. |
| `deeplinks` | no | `[]` | Links for the `l` quick-picker — each a URL string or `{ "name", "url" }`. `<name>` / `{name}` placeholders (e.g. `myapp://?redirect=RC&uuid=<uuid>`) are asked for when you pick the link. |

Only `name` and `root` are mandatory — and `rn init` fills both in for you.

## The dashboard

Three tiled panes plus a status bar:

- **Processes** — Metro and each install/run, one sub-tab each, shown as a live
  terminal (colors and Metro's interactive menu render faithfully).
- **Devices & Actions** — every installed simulator/emulator and connected
  physical device, with a running marker. On **Android**, when `android.bundleId`
  is set, each running device also shows whether the app is installed (`app✓`/
  `app✗`) and whether it's in the foreground (`▶fg`). (iOS can't report these over
  the available tooling, so they're Android-only.)
- **JS Logs / Network / Perf** — the full `metroctl logs` viewer embedded (see
  [docs/logs.md](docs/logs.md) for its keys).

### Keys

Each pane owns its own keys (shown in that pane's footer); only a few are global.

**Global** (status bar):

| Key | Action |
|---|---|
| `⇥` / `⇧⇥` | move focus between panes |
| `Ctrl`+`←→↑↓` | resize the panes (horizontal / vertical split) |
| `R` / `D` | send Metro reload / dev-menu (goes to the Metro process) |
| `q` / `Ctrl-C` | quit — asks `y/n` first, then stops the processes simon started |

**Processes pane:**

| Key | Action |
|---|---|
| `[` / `]` | switch sub-tab (Metro / iOS / Android) |
| `⏎` | enter **input mode** — raw keys go to the process (then `esc` to leave) |
| `x` | stop (kill) the process in the current tab |
| `m` | start (or restart) Metro |

**Devices pane:**

| Key | Action |
|---|---|
| `↑↓` / `jk` | select a device |
| `⏎` | install & run the app on it — targets *that* device (iOS `--udid`, Android `--deviceId`); an offline sim/emulator is launched first |
| `b` / `s` | start (boot) / stop (shut down) the selected simulator or emulator |
| `o` | launch the app on the device by its `bundleId` |
| `l` | pop up the `deeplinks` picker; press the number/letter beside a link to open it on the device (a form asks for its placeholders), or `/` to paste any URL, e.g. a magic login link |

Each pane's keys show in its own footer, and only the focused pane's footer is lit
— the others go dark so there's no clutter.

**Driving Metro.** Focus the Processes pane, press `⏎`, and every key goes
straight to Metro — exactly like a normal terminal, so `r`, `d`, `j` and anything
else Metro supports work. `esc` returns to navigation, so you're never trapped.
Don't want to switch panes? `R` / `D` send reload / dev-menu from anywhere (`R`
falls back to a CDP reload when Metro isn't running under simon).

> **Dev-only, macOS.** The JS feed needs Metro running; booting simulators and
> the app-presence checks use the same `xcrun`/`adb` tooling as the rest of simon.
> Foreground detection is Android-only; iOS can't report it over these tools.

## Isolated sessions (`up` / `down`)

`metroctl up` opens the dashboard and gets the app running on its own port and
simulator, so several checkouts (e.g. git worktrees) can run side by side:

```sh
metroctl up --port auto --new-sim     # free port, new simulator, Metro, build
metroctl up --port 8090 --device <udid>
metroctl down                         # from another shell: stop it, delete its simulator
```

- `--port auto` picks the first free port from the configured one, skipping
  ports other sessions have claimed. The port reaches the build through
  `RCT_METRO_PORT`, so each app talks to its own Metro.
- `--new-sim [NAME]` creates a simulator (default `metroctl-<dir>`) using the
  newest iOS runtime and the newest plain iPhone. It's cloned from a settled
  template (`metroctl-template …`, built once per device type and iOS version),
  because a freshly created simulator takes minutes before it can launch apps. Override them with
  `--sim-type "iPhone 16 Pro"` / `--sim-runtime 18.2`. On quit, metroctl asks
  whether to delete it (`--sim-cleanup ask|delete|keep`).
- `--install` installs JS deps and pods first.
- `--prebuilt` skips the native build: it installs the app the main checkout
  last built (newest matching bundle id in Xcode's DerivedData) and points it at
  this session's Metro, so a new worktree is running in seconds. Use `rebuild`
  (or `b`) after native changes. Falls back to building when there's no build.
- Steps run in order: install and simulator boot run in parallel, then Metro
  starts, then the build runs once Metro answers. Each step gets its own
  process tab.
- In a git worktree of a registered project, metroctl uses that project's
  config with its root moved into the worktree. No `init` is needed.
- The running session is described in `.metroctl/session.json` (pid, port,
  udid, status), which is git-excluded.

- Sessions that die without cleaning up (killed, crashed) are reaped the
  next time `up` runs, or with `metroctl gc`: their simulator is deleted if
  metroctl created it, otherwise the app is stopped and its port setting reset.
  `gc --sims` also deletes `metroctl-*` simulators no running session owns.

## Agents (`metroctl mcp`)

Every dashboard serves a control socket (path in `.metroctl/session.json`).
`metroctl ctl <cmd> [json]` sends it one command; `metroctl mcp` is an MCP
server on top of it for coding agents:

```json
{ "mcpServers": { "metroctl": { "command": "metroctl", "args": ["mcp"] } } }
```

| Tool | |
|---|---|
| `status`, `wait_ready` | session state; block until the app runs (or the build fails) |
| `logs`, `errors`, `network`, `request` | JS console, failures, requests (`since` for only new entries) |
| `output` | last lines of a process tab (build errors) |
| `reload`, `rebuild`, `restart_metro` | drive the session |
| `deeplinks` | the configured deep links (open them with `touchctl open`) |

To see and use the app's screen (screenshots, element tree, taps, typing,
deep links), use [touchctl](https://github.com/hvalec427/touchctl). Point it at
the session's device with `touchctl --device-from .metroctl/session.json …`.

## Contributing

Commits must follow [Conventional Commits](https://www.conventionalcommits.org)
(`feat: …`, `fix(rn): …`) — release versions are derived from them. Enable the
local check once per clone:

```sh
git config core.hooksPath .githooks
```
