# React Native logs (`signalbox logs`)

React Native **JS console + network** from Metro's inspector (CDP) — the same feed React Native DevTools uses — for any device/simulator connected to Metro (no go-ios/adb needed). This is the standalone viewer; the [dashboard](../README.md) embeds the same thing.

```sh
signalbox logs [--port 8081]
```

Dev-only (Metro must be running); network depth depends on your RN version's CDP support. Piped or redirected, it falls back to a plain line stream.

In a terminal it opens an **interactive viewer** (dark theme) with **Logs**, **Network**, **Perf** and **Components** tabs. The Logs/Network/Components tabs show two panes side by side: the list, and the detail of its selected row. `⇥` (or `⏎`) moves focus to the detail pane, `⇥` or `esc` back to the list. In the dashboard, `⇥` goes through both along with the dashboard's other panes. Narrower than 60 columns, the focused pane takes the whole width.

## Perf tab

A live dashboard for the active device, sampled once a second:

- **JS-thread FPS** — how many frames per second the JS thread services (≤60), shown as a number, a bar, and a rolling sparkline of the last 60s. signalbox measures this by injecting a `requestAnimationFrame` counter into the app over CDP.
- **JS heap** — used / total, via `Runtime.getHeapUsage` (shown only if the runtime reports it).

> Native/UI FPS is **not** exposed over CDP, so only the JS-thread figure is available here — the same "JS" number RN's in-app perf monitor shows. Switch devices with `1`-`9`.

## Components tab

The app's React component tree, like the Components panel in React Native DevTools (it talks to the same React DevTools backend inside the app, so it needs React Native 0.76+ in a dev build).

| Key | Action |
|---|---|
| `s` | select a component by tapping it in the app (`s` again cancels) |
| `↑↓` / `jk` | move; the component under the cursor is outlined on the device until you leave the tab |
| `h` / `←` | fold the component, or go to its parent |
| `l` / `→` | unfold it, or go to its first child |
| `u` | go to the parent |
| `o` | open where it's used in nvim: the JSX line in the parent that rendered it |
| `O` | open where it's defined |
| `⏎` | in the detail pane: edit the prop on the cursor line (see below), or open the file of a `used in`/`defined in` line |
| `/` · `f` | search · filter by component name |

The detail pane shows the selected component's props, state, hooks, context, errors, who rendered it, and where it's used and defined.

- The inspected component refreshes every second, so props and state stay current.
- Values the backend sends as a preview (deep objects) are fetched once more, so props read a level or two deeper; past that they show as `{…}`.
- **Editing (experimental).** In the detail pane, move to any line under Props, at any depth, and press `⏎`: the prompt reads `path = value` with the prop's path (`style.0.color`) and, when it's plain data, its current value as JSON. Change the value (JSON, or else plain text) and press ⏎; you can change the path too. Functions and elements show no value, but can still be replaced, e.g. with `null`. An edited prop is marked `✎` with the value it had, e.g. `✎ color: "red"  (was "#1C2026")`. In React DevTools an edited prop lasts only until the parent renders again; signalbox keeps it, until you put the old value back, reload, or the component leaves its place in the tree. `SIGNALBOX_PIN` picks how edits are kept:
  - `a` (default): after each render, re-apply an edited prop the parent reset. Works for any component.
  - `b`: swap the component's type for a wrapper that passes its edited props on every render, the way Fast Refresh swaps edited code. Function components only; editing the component's own file remounts it.
  - `off`: plain React DevTools edits.
- The backend talks to one frontend at a time, so signalbox connects only while this tab is open. Opening the Components panel in React Native DevTools (`j` in Metro) takes it over; switch tabs and back to take it back.

### The list

With the list focused:

| Key | Action |
|---|---|
| `[` / `]` | switch tab |
| `1`-`9` | switch device |
| `↑↓` / `jk` / PgUp·PgDn / `g`·`G` | move |
| `/` | search (jumps live to the first match; `n`/`N` next/prev) |
| `f` | filter (Network: matches status code, method, url, duration, and GraphQL op) |
| `y` | copy the selected row (a log — with its full expanded object/array tree — or a request's full detail) |
| `V` | start a visual selection — `j`/`k` extend it over rows, `y` yanks them all (`V`/`esc` cancels) |
| `space` / `a` | toggle autoscroll (stick to newest) |
| `c` | clear |
| `p` | keep-vs-clear logs on app restart |
| `R` | reload the app |
| `r` | reconnect |
| `q` | quit |
| `e` | *(Network)* toggle errors-only (4xx/5xx) — footer shows `errors:on/off` |
| `m` | *(Network)* cycle the HTTP-method filter (ALL → GET → POST → PUT → PATCH → DELETE → HEAD → OPTIONS) — footer shows `method:<current>` |

`HEAD` and `OPTIONS` are standard HTTP methods you'll see in real traffic (`HEAD` fetches only headers, no body; `OPTIONS` is a CORS/preflight probe). The active `e`/`m` filters also appear in the header bar (e.g. `errors+POST`).

The detail pane follows the selected row; `J`/`K` scroll it without leaving the list.

### The detail pane

With the detail pane focused (`⇥` or `⏎`), the keys drive it: `j`/`k`/`g`·`G`/PgUp·PgDn move a line cursor, `{`/`}` jump between sections, `/` + `n`/`N` search within it, **`c` copies** (a network request → `curl`), `y` copies the whole pane. **`⏎`** acts on the cursor line: a `file:line` (a stack frame, a component's source) opens in nvim, anything else is copied. Press **`V`** to start a line selection — `j`/`k` extend it, `y` yanks just those lines. `esc` goes back to the list.

### Search, objects, and the Network tab

- Search highlights **every** match — the active one (where the selection / `n`·`N` sits) in **yellow**, the rest in **cyan**.
- **Where search looks depends on the focused pane.** In the list, `/` jumps live to the first match and `n`/`N` step the selection through every match (including ones in the status, duration, or GraphQL-op columns). In the detail pane, `/` searches the pane from its top and `n`/`N` step through the pane's matches; the list stays put.
- A **log with an object** shows the full nested object tree, fetched lazily via `Runtime.getProperties`; nested previews already present in the log event render inline for free.
- A **network** row shows method, URL, status, **duration**, response **size**, and (in the pane) request/response **headers and bodies** (JSON pretty-printed). Long values wrap so nothing is cut off. **Failed** requests show `FAIL` and the error reason.
- **WebSockets** appear as `WS` rows; the pane lists each frame with a direction arrow (`↑` sent, `↓` received) — handy for GraphQL subscriptions.
- An **uncaught exception / `console.error`** shows its call stack (`function (file:line)`) in the detail pane.
- **GraphQL** POSTs show their operation name right in the Network list (e.g. `200 POST …/graphql  GetOrders`), taken from `operationName` or the query. Requests also show their timing (e.g. `123ms`).
- The header bar shows the active target's engine (e.g. `Hermes`) alongside its name and connection status.

It **auto-reconnects** on close/crash and honours the clear-on-restart toggle on fast-refresh.

### Attaching an external debugger (`--print-ws`)

signalbox doesn't do breakpoint debugging, but it can hand you the Metro inspector WebSocket URL so another tool (e.g. `nvim-dap` via `vscode-js-debug`'s *attach*) can connect:

```sh
signalbox logs --print-ws                 # print URL(s) for every target
signalbox logs --print-ws "iPhone 16"     # just the matching target
WS=$(signalbox logs --print-ws)           # capture it for a debugger config
```

URLs go to stdout (so it's pipeable); when several targets match, each is labelled on stderr. The breakpoints/stepping themselves live in your editor's DAP setup — signalbox only points it at the right endpoint.
