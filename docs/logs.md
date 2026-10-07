# React Native logs (`metroctl logs`)

React Native **JS console + network** from Metro's inspector (CDP) — the same feed React Native DevTools uses — for any device/simulator connected to Metro (no go-ios/adb needed). This is the standalone viewer; the [dashboard](../README.md) embeds the same thing.

```sh
metroctl logs [--port 8081]
```

Dev-only (Metro must be running); network depth depends on your RN version's CDP support. Piped or redirected, it falls back to a plain line stream.

In a terminal it opens an **interactive viewer** (dark theme) with **Logs**, **Network**, and **Perf** tabs. The Logs/Network tabs have two modes (list and preview).

## Perf tab

A live dashboard for the active device, sampled once a second:

- **JS-thread FPS** — how many frames per second the JS thread services (≤60), shown as a number, a bar, and a rolling sparkline of the last 60s. metroctl measures this by injecting a `requestAnimationFrame` counter into the app over CDP.
- **JS heap** — used / total, via `Runtime.getHeapUsage` (shown only if the runtime reports it).

> Native/UI FPS is **not** exposed over CDP, so only the JS-thread figure is available here — the same "JS" number RN's in-app perf monitor shows. Switch devices with `1`-`9`.

### List mode (default, and the side-by-side split)

Keys drive the list:

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

**`⏎`** opens a **side-by-side detail pane** for the selected row (list left, details right). `j`/`k` still move the list and the pane follows; `J`/`K` scroll the pane; `⏎`/`esc` closes.

### Preview mode (`z` — maximized)

**`z`** maximizes the pane to full width (list hidden). Now the keys drive the pane: `j`/`k`/`g`·`G` move a line cursor and `J`/`K`/PgUp·PgDn scroll, `/` + `n`/`N` search within it, **`c` copies** (a network request → `curl`), `y` copies the whole pane. Press **`V`** to start a line selection — `j`/`k` extend it, `y` yanks just those lines. `z` returns to the split.

### Search, objects, and the Network tab

- Search highlights **every** match — the active one (where the selection / `n`·`N` sits) in **yellow**, the rest in **cyan**.
- **Where search looks depends on the pane.** With the preview **closed**, `/` searches the list — it jumps live to the first match and `n`/`N` step the selection through every match (including ones in the status, duration, or GraphQL-op columns). With the preview **open** (split *or* maximized), `/` searches the pane from its top and `n`/`N` step through the pane's matches; the list stays put.
- A **log with an object** shows the full nested object tree, fetched lazily via `Runtime.getProperties`; nested previews already present in the log event render inline for free.
- A **network** row shows method, URL, status, **duration**, response **size**, and (in the pane) request/response **headers and bodies** (JSON pretty-printed). Long values wrap so nothing is cut off. **Failed** requests show `FAIL` and the error reason.
- **WebSockets** appear as `WS` rows; the pane lists each frame with a direction arrow (`↑` sent, `↓` received) — handy for GraphQL subscriptions.
- An **uncaught exception / `console.error`** shows its call stack (`function (file:line)`) in the detail pane.
- **GraphQL** POSTs show their operation name right in the Network list (e.g. `200 POST …/graphql  GetOrders`), taken from `operationName` or the query. Requests also show their timing (e.g. `123ms`).
- The header bar shows the active target's engine (e.g. `Hermes`) alongside its name and connection status.

It **auto-reconnects** on close/crash and honours the clear-on-restart toggle on fast-refresh.

### Attaching an external debugger (`--print-ws`)

metroctl doesn't do breakpoint debugging, but it can hand you the Metro inspector WebSocket URL so another tool (e.g. `nvim-dap` via `vscode-js-debug`'s *attach*) can connect:

```sh
metroctl logs --print-ws                 # print URL(s) for every target
metroctl logs --print-ws "iPhone 16"     # just the matching target
WS=$(metroctl logs --print-ws)           # capture it for a debugger config
```

URLs go to stdout (so it's pipeable); when several targets match, each is labelled on stderr. The breakpoints/stepping themselves live in your editor's DAP setup — metroctl only points it at the right endpoint.
