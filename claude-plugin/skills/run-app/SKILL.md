---
name: run-app
description: Run this React Native checkout's app on its own simulator and check changes in it. Use before changing UI or behaviour you'll need to see in the app, and when asked to run, screenshot, tap through or test the app.
allowed-tools: Bash(signalbox *) Bash(boop *) mcp__plugin_rn-sim_signalbox
---

# Run the app

This checkout gets its own simulator and Metro port. signalbox hands them out, so no other
session (another agent, another worktree) ever shares them, and boop drives only this
checkout's simulator. Run every command from inside the checkout; signalbox's tools and
boop find its session by themselves, so they take no `--device`.

## Start it

1. Run `signalbox up --detach --port auto --new-sim --prebuilt`. Done when it prints
   `port … · simulator …` (just started) or `already running: …`.
   - The first run on a Mac builds a simulator template, a few minutes; the command waits.
   - "isn't a registered React Native project": ask the user to run `signalbox init` once in
     the main checkout (it asks for a name), then run it again.
   - Any other failure prints signalbox's output: fix what it names (pods out of date:
     `cd ios && pod install`) or report it.
2. Call signalbox's `wait_ready` tool. Done when it reports the app running. A failed build:
   read `errors` and `output`, fix it, then `rebuild`.

## Check your work

- The screen, through boop: `boop screenshot` prints a JPEG path, read it; `boop ui` lists
  elements with #testID, "label" and @x,y; `boop tap --id <testID>` (or `--label <text>`, or
  `x y`); `boop swipe up|down|left|right`; `boop type <text> --id <field>`;
  `boop press home|enter`; `boop open <url>` for deep links.
- After JS changes Metro reloads the app: read signalbox's `errors` and `logs`; `reload` when
  the JS state is stale.
- Native changes (Podfile, `ios/`, native modules): `cd ios && pod install` if pods changed,
  then `rebuild`. Metro config or JS dependencies: `restart_metro`.
- Deep links: `deeplinks` lists the app's links with <placeholders>. A magic login link or
  token from the task: open it with `boop open`, then check the screen.
- API calls: `network` and `request`.

## Done means seen

A change is done when you've seen it working: every screen you changed looked at, the flow
walked when it's interactive, `errors` read. Run the project's own checks too (typecheck,
lint, tests) and fix what your change broke. Report as working only what you saw working;
name what you couldn't verify and why (no test account, backend down, build failing).

## Leave it running

The app stays up between turns, so the next check is quick. It stops, and its simulator is
deleted, when this Claude session ends; `signalbox down` stops it sooner. The user can
watch it with `tmux -L signalbox attach` (a window named after the checkout).
