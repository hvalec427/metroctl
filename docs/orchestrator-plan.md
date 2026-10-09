# Orchestrator plan

**orc** is a TUI for working on several features of a React Native app at
once. Each feature request gets its own git worktree, Metro port, iOS
simulator and Claude Code agent. You talk to every agent from the orc TUI.
metroctl does the React Native work, and it stays a standalone tool that
works without orc.

Scope for now: iOS simulators only, Claude Code only.

## Layout

```
tmux session "orc"
├─ window 0: orc TUI              (requests list + agent chat; normal work happens here)
├─ window "feature-a"             (metroctl up …, only for manual debugging)
└─ window "feature-b" …
```

- The agents run under **orcd**, a background daemon (`orc daemon`), as
  `claude` child processes. The orc TUI is a client of orcd: it shows the
  agents' messages and sends replies, so you don't need to switch windows for
  normal work. Quitting the TUI leaves the agents working. Reopening it
  reattaches and replays each conversation from orcd.
- metroctl runs in its own tmux window for each request. You only go there to
  debug by hand (`⏎`/`g` in orc jumps there). Its Metro, simulator and logs
  keep running while you're in orc.
- When started outside tmux, orc creates or attaches to the `orc` session and
  re-launches itself in window 0.

## orcd (daemon)

- It's the same binary: `orc daemon`. The TUI starts it if it isn't running.
  It runs detached from the terminal (`setsid`) or as a hidden `orcd` window
  in the tmux session. A launchd agent can come later if it should survive
  logout or reboot.
- It owns everything long-lived: the `claude` processes, their transcripts,
  the request registry, worktree/metroctl setup and teardown, and the
  permission-prompt MCP tool (approvals wait in orcd until you answer them in
  the TUI).
- The TUI talks to it over `~/.config/orc/orcd.sock` (JSON lines). It sends
  commands (new request, send message, approve/deny, tear down) and
  subscribes to events (agent messages, status changes).
- It persists the registry and each agent's `session_id`. If orcd itself is
  restarted, it resumes the agents with `claude --resume`.

## Repo and paths

- orc lives in `/Users/hvalec/dev/orc`. The existing code gets deleted and orc
  starts over. Its stream-json driver (`src/agent/cli_driver.rs`,
  `stream.rs`) is a useful reference.
- Worktrees go in a sibling folder of the project:
  `<parent>/<project>-worktrees/<slug>`. For example, a project in
  `/Users/hvalec/dev/app` gets worktrees in `/Users/hvalec/dev/app-worktrees/`.

## Request lifecycle (orc)

1. **New request:** pick the project, enter a title and a prompt. The slug
   comes from the title.
2. **Worktree:** `git worktree add <project>-worktrees/<slug> -b <slug>`.
3. **Dependencies:** copy `node_modules` and `ios/Pods` from the main checkout
   (`cp -c`, an APFS clone, so it's fast and takes no extra space). If the
   agent later changes dependencies, it runs `yarn install` / `pod install`
   again.
   Also copy the git-ignored files the build needs. In laundryheap-mobile
   that's `.env` and `ios/.xcode.env.local`. The list should be per-project
   config in orc (e.g. `copy: [".env", "ios/.xcode.env.local"]`). Then run
   `pod install`, which regenerates `ios/build` (React Native codegen) for
   the worktree's own branch. Copying `ios/build` instead works, but goes
   stale if the branch changes native code. Measured on laundryheap-mobile:
   cloning `node_modules` 19s, `ios/Pods` 8s, then `pod install` 27s.
4. **metroctl window:** `tmux new-window -d -n <slug> -c <worktree>
   'metroctl up --port auto --sim new'`. This picks a port, creates and boots
   a simulator, starts Metro and builds onto the simulator.
5. **Agent:** orcd starts `claude -p --input-format stream-json --output-format
   stream-json --verbose` in the worktree with the prompt. Its `.mcp.json`
   points at `metroctl mcp`. Store the `session_id` so the agent can be
   resumed with `--resume` after orc restarts.
6. **Track:** status comes from the agent stream plus
   `.metroctl/session.json` and metroctl's control socket. Statuses: setting
   up → building → agent working → waiting for you → done.
7. **Review:** read the chat, look at the simulator, view the diff, open a PR.
8. **Tear down:** `metroctl down` (stops Metro, deletes the simulator), kill
   the window, `git worktree remove`, and optionally delete the branch.

orc's own state is a small registry (`~/.config/orc/requests.json`: slug,
project, worktree, branch, claude session id, status). Everything live is read
back from the processes.

## orc TUI

- **Left:** a list of requests with status badges (working, waiting for you,
  error, build failed) and their port and simulator.
- **Right:** the selected agent's conversation (assistant text, tool calls
  collapsed to one line, results on demand) with an input box. `⏎` sends.
- **Permission prompts:** the agent's tool approvals are shown in orc. Use
  `--permission-prompt-tool` (an MCP tool served by orc) so approve/deny
  happens in the TUI, or start agents with a pre-approved allowlist.
- **Keys:** `n` new request, `g` go to the metroctl window, `d` diff,
  `x` tear down, `?` help.

## How the agent uses metroctl

- `.mcp.json` in the worktree runs `metroctl mcp`, a stdio MCP server. It
  finds the running metroctl through `.metroctl/session.json` and talks to it
  over the control socket.
- Tools: `status`, `logs(level, filter, since)`, `errors`,
  `network(failed_only, filter)`, `reload`, `rebuild`, `restart_metro`,
  `screenshot`, `open_deeplink`. Later: tap/type through Maestro or idb.
- Outputs are compact and filtered so they don't fill the agent's context.

## metroctl changes (do first, all optional for standalone use)

1. **Worktree project resolution.** If the cwd isn't registered, find the main
   checkout with `git rev-parse --git-common-dir` and use its project config
   with `root` = cwd.
2. **`metroctl up`**, the dashboard plus startup flags:
   - `--port <n|auto>`: override `metro.port` (and therefore `RCT_METRO_PORT`).
     `auto` picks the first free port starting at 8081.
   - `--device <udid>`: preselect and pin to that device.
   - `--sim new[=<name>]`: create a simulator (simon `create_simulator`, using
     the newest runtime and a default iPhone, both overridable), boot it, and
     pin to it. Name it `metroctl-<dir>`.
   - `--install`: run `install_command()` first. orc copies dependencies
     instead, so it doesn't pass this.
   - Once set up: start Metro, then build onto the pinned device after it has
     booted. Each step runs as a normal process tab.
3. **Session file:** write `.metroctl/session.json` (pid, project root, port,
   udid, created_sim, socket path, status) and remove it on exit. Ignore it
   through `.git/info/exclude`, so the project's `.gitignore` isn't touched.
4. **Simulator cleanup:** on quit, delete a simulator metroctl created
   (`--sim-cleanup=delete|keep|ask`, default ask). Add `metroctl down` to stop
   it from outside.
5. **Control socket:** a Unix socket at `.metroctl/control.sock` that speaks
   JSON lines. It serves status, logs, errors and network from the dashboard's
   in-memory buffers, plus reload, rebuild and restart_metro.
Learned while testing on laundryheap-mobile: with React Native's prebuilt
core, the port passed at build time (`RCT_METRO_PORT`) is ignored and the app
still loads from 8081. metroctl therefore sets `RCT_jsLocation=localhost:<port>`
for the app on the simulator after every successful iOS simulator build, and
relaunches the app if the setting changed. This needs `ios.bundleId` in the
config.

6. **`metroctl mcp`:** a stdio MCP server that forwards to the control socket,
   plus `screenshot` (`xcrun simctl io <udid> screenshot`).

Plain `metroctl` with no flags behaves exactly as it does today.

## Next: agents control simulators and devices

Add UI control to `metroctl mcp`, so an agent can drive the app it's working
on, not just look at it. No Maestro.

| Target | Screen | Element tree (ids, text, bounds) | Tap / swipe / type |
|---|---|---|---|
| iOS simulator | `xcrun simctl io <udid> screenshot` | AXe `axe describe-ui --udid` (or idb `ui describe-all`) | `axe tap`, `axe swipe`, `axe type` |
| iOS device | WebDriverAgent `GET /screenshot` | WebDriverAgent `GET /source` (accessibility identifiers) | WebDriverAgent tap / drag / keys |
| Android device + emulator | `adb exec-out screencap -p` | `adb shell uiautomator dump` (resource-id, text, content-desc, bounds) | `adb shell input tap/swipe/text` |

- New tools: `ui` (a compact element tree: role, id/testID, label, center
  point; filterable), `tap` (by id, label or x/y), `swipe`/`scroll`,
  `type_text`, `press` (home/back/enter).
- Tapping by id or label resolves through the tree, so agents use testIDs
  instead of guessing coordinates. Coordinates are in points, the same space
  `ui` reports.
- The session's pinned device decides the backend. Sessions on real devices
  and Android need `--device` pinning to cover those (today `up` only creates
  iOS simulators).
- iOS devices need WebDriverAgent built and running on the phone
  (`xcodebuild test-without-building` with the WDA runner, port-forwarded with
  `iproxy`). metroctl should start and track it like the other processes.
- Check tools at startup and report what's missing (`brew install
  cameroncooke/axe/axe`, adb from the Android SDK, WDA).
