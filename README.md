<!-- LOGO -->
<h1>
<p align="center">
  <img src="https://rioterm.com/assets/rio-logo.png" alt="Rio terminal logo" width="128">
  <br>Rio Terminal — cantona edition
</h1>
  <p align="center">
    A highly optimized, highly customizable fork of the GPU-accelerated
    Rio terminal — with a built-in session manager that keeps your shells
    alive after you quit, output-driven automation triggers, working
    inline images, and a tab bar you actually control.
    <br />
    <a href="#why-switch">Why switch</a>
    ·
    <a href="#the-killer-feature-session-manager">Session manager</a>
    ·
    <a href="#more-killer-features">More features</a>
    ·
    <a href="#build--install">Build & Install</a>
    ·
    <a href="#configuration">Configuration</a>
  </p>
</p>

A fork of [raphamorim/rio](https://github.com/raphamorim/rio) that keeps every bit
of Rio's GPU-fast, damage-tracked rendering core and bolts on the features a
daily-driver terminal is missing: a real session manager, output-driven
automation, inline images that don't fall over, and a tab bar with more than
two knobs. Tracks upstream `main` and rebases regularly — everything upstream
ships, this fork has, plus everything below. `dev` is the default branch and
the product.

## 🚀 Why switch

- **Your shells outlive the terminal.** Quit rio, or let it crash — `vim`,
  `top`, a long build, an SSH session, all still running when you come back.
  Not a scrollback replay: the actual process, reattached live. No tmux, no
  prefix key.
- **Your workspace survives restarts either way.** Tabs, split layout and
  ratios, per-pane working directory, window size and styled scrollback come
  back exactly as you left them, including named workspaces.
- **Regex rules watch your output so you don't have to.** Auto-login a serial
  console, fire a desktop alert on an error pattern, recolor a tab when a
  build finishes — six actions, hot-reloaded from a plain TOML file.
- **Inline images render and stay rendered.** Sixel, iTerm2 and kitty
  graphics render correctly and hold up under repeated large images —
  no drift, no GPU-memory creep over a long session.
- **The tab strip is actually configurable.** Ten `[navigation]` knobs for
  geometry, fill and hover behavior, applied live the moment you save, plus a
  misclick guard so a stray click doesn't eat your tab.
- **Bugs get fixed fast.** This is an actively, frequently maintained tree:
  the moment something misbehaves it's tracked and fixed, not left to sit.
  You get upstream's polish on a shorter feedback loop.
- **None of it costs you frames.** Every addition is built on top of Rio's
  per-line damage tracking and is perf-first, not bolted on. Existing configs
  keep working unchanged, and every enhancement is one config line to tune or
  turn off.

## 🧬 The killer feature: session manager

Rio now ships a tmux-style session manager as a first-class part of the
terminal — not a wrapper, not a separate program you have to remember to
launch. Two orthogonal levels control it.

### Level 1 — `restore`: when the session comes back

Close the terminal, reopen it, and your world comes back — like a browser
restoring tabs. Every tab, the split layout with its ratios, each pane's
working directory, the window size, even the styled scrollback text.

```toml
[session]
# "disable" (default) turns sessions off entirely.
# "prompt" asks first — "save?" when a window closes or rio quits,
#   "resume?" at launch — and a yes does exactly what "always" would.
# "always" does it all silently, and autosaves on tab/split changes.
restore = "prompt"
# How many scrollback lines each pane saves. Default: 2000.
max-scrollback-lines = 2000
```

- `ctrl+shift+s` saves on demand, with a "session saved" flash.
- Named workspaces: `rio --session work`, or the command palette's session
  save/restore actions.
- The launch resume prompt offers three choices: `r` resume the saved
  session, `n` start new and discard the old one (its persistent daemons, if
  any, are killed — not stranded), or `k` start new but keep the old session
  running in the background so a later launch can still resume it.

### Level 2 — `persistent`: dead scrollback or live shells

This is the fork's headline feature. `restore` alone (level 1) gives you a
browser-tab-style restore: layout and cwd come back, but each pane is a
**fresh shell** with the old output repainted above the prompt — the process
itself did not survive. Turn on `persistent` and it does.

```toml
[session]
restore = "prompt"
# false (default) = v1: fresh shells, old output repainted.
# true = v2: live shells that outlive rio via rio-ptyd.
persistent = true
# Per-pane replay buffer, in bytes, that repaints the screen on
# reattach. Default: 1048576 (1 MiB).
persistent-ring-bytes = 1048576
```

With `persistent = true`, every pane runs behind a standalone daemon,
`rio-ptyd`, that owns the real PTY instead of rio owning it directly. Quit
rio — or let it crash — and the shells keep running. Relaunch, and rio
reattaches to the live processes and replays their screens: `vim`, `top`,
your build, your `ssh` session, all still there, still running, exactly
where you left them.

And closing is just as deliberate as resuming. Rio reads each close for
what it means and cleans up — or preserves — accordingly:

| How you close a window | Your session | Your shells |
|---|---|---|
| `exit`, or close the last tab while idle | tidied out of the session | ended — nothing lingers behind |
| close the last tab mid-work (an editor, a build, an ssh session) | kept | keep running — the next launch reattaches with the program still going |
| the window's ✕ button | kept | keep running, reattach on relaunch |
| quit rio — or crash | saved as-is | all keep running, reattach on relaunch |

(Closing an individual tab or split *inside* a window always ends just
that shell — deliberate is deliberate.) Fat-finger `alt+w` on a window
running a build and nothing is lost; close an idle one and nothing is
leaked — `rio-ptyd list` stays exactly as long as your real workload. In `prompt` mode rio asks before acting and your
answer does precisely the above; in `always` mode it simply happens.

This is the pitch in one line: **no tmux, no prefix keys, native GPU
rendering, and the session actually survives.**

`rio-ptyd` is a self-contained binary — it has no dependency on rio itself,
just `libc` and `serde` — so a session is inspectable and scriptable from any
terminal, rio or otherwise:

```
$ rio-ptyd list
PANE ID   STATE         PID  SESSION  ACTIVITY        CREATED
8bd31f72  running     40231  [work]   vim             2026-07-15 21:04
ca203ac5  running     40988  [work]   /home/you/src   2026-07-15 21:11
```

- `rio-ptyd list [--json] [--full] [--sort session|created|id|pid]` — every
  live pane: session, what it's doing right now (the foreground program, else
  its working directory), and when it started. Grouped by session by default;
  a short pane id is shown (use `--full` for the whole 32-hex, or when two
  ids share a prefix it widens automatically).
- `rio-ptyd attach <id>` — reattach to a pane by hand. Any command that takes
  an id accepts the short prefix from `list`, not just the full 32-hex.
- `rio-ptyd kill <id>` — end a pane's shell and daemon.
- `rio-ptyd kill-session <name | --unnamed>` — end a whole workspace at once:
  every pane of a named session, or every untagged one. `--dry-run` shows
  what it would kill first.
- `rio-ptyd gc` — clean up daemons with no reachable owner, including ones
  whose shell has exited but that linger waiting for a reattach.

**Remote sessions over SSH.** Because `rio-ptyd` speaks a byte protocol
instead of passing file descriptors, a pane hosted on another machine
attaches exactly like a local one. The command palette's "Attach Remote
Pane…" prompts for a `user@host`, lists that host's live panes over SSH, and
opens the one you pick as a new tab — repaint and all. Remote tabs are saved
with their host and reattach over SSH on the next restore, same as a local
pane reattaches to its local daemon. Closing a remote tab kills its shell
like any other; what rio never does implicitly is reap a remote shell when
you *decline* a session save or when `gc` cleans up — those touch local
daemons only.

## ✨ More killer features

### ⚡ Terminal triggers — iTerm2-style automation, built in

Regex-to-action rules in a hot-reloading `triggers.toml`. Six actions:

```toml
# triggers.toml has the rule list at the top level — [[rules]], not
# nested under a [triggers] table.
[[rules]]
regex = "login:"
once = true            # one-shot; re-arm with the resettriggers action
[rules.action]
send_text = { text = "admin\n" }

[[rules]]
regex = "error: (.*)"
[rules.action]
notify = { title = "Error", body = "\\1", urgency = "critical" }
```

- **Highlight** matched text, with a per-rule color.
- **Recolor the tab** — flag a build failure or a finished job at a glance.
- **Desktop notify**, with urgency levels (`low` / `normal` / `critical`).
- **Run a command**, detached, on match.
- **Pipe the screen to a coprocess** — the visible screen, not just the
  matched line, so the command can see multi-line context.
- **Type text back into the pty** — auto-answer a prompt.

Rules scan the focused pane as it renders; a background tab's output is
picked up when you switch to it. When a capture feeds a command's
arguments, put `--` before it so hostile output can't smuggle an option
(`args = ["--", "\1"]`).

A typo never costs you a working terminal. Break `config.toml` mid-edit
and rio keeps running on the live config instead of resetting to
defaults; break `triggers.toml` and the running rules stay live. Both
surface the parse error as an in-terminal overlay — fix the file, save,
and the overlay clears itself.

Two flags refine when a rule fires: `once` (fire a single time, until the
config reloads — good for a one-shot login probe) and `instant` (fire
mid-line, on every output batch, instead of waiting for the line to be
finalized). Enough to script a full serial-console auto-login — detect the
`login:` prompt, send credentials — or turn any log pattern into a desktop
alert.

### 🖼️ Inline images that actually work

Sixel, iTerm2 (OSC 1337) and kitty graphics render correctly: `imgcat`,
`chafa -f iterm`, `chafa -f sixels`, `chafa -f kitty`, `viu`, TIFF payloads
included. The hard part is staying correct under real use, and that's where
this fork puts its attention:

- **No leak, no drift under load.** Repeated large images hold steady —
  GPU textures are evicted once their pixels are gone, so a long session
  doesn't creep upward in memory.
- **kitty placements stay inside the grid.** An image whose top has
  scrolled above the viewport is clipped at the grid edge instead of
  bleeding up into the tab bar.
- **Overlapping and side-by-side images don't corrupt each other**, and
  deleting one leaves its neighbor intact.
- **Images survive a window resize in one piece.** Rows that anchor a
  picture never rewrap — no torn bands, no ghost copies — and a shrink
  only hides pixels: drag the window narrower and back, and the whole
  picture is still there.
- **The prompt lands below the picture, every time.** Sixel streams that
  end with a graphics-new-line — the way `chafa` and `img2sixel` finish —
  leave the cursor under the image, so the second render is as clean as
  the first.

These are the kind of rough edges that surface only when you lean on the
feature daily — found in use, fixed quickly, kept fixed.

### 🎨 A tab bar you fully control

Every dimension of the tab strip is a `[navigation]` config value, applied
live the moment you save:

```toml
[navigation]
# Height of the tab strip, logical px. Default: 38.
tab-bar-height = 38
# Tab title font size, logical px. Default: 12.
tab-font-size = 12
# Widest a single tab may grow, logical px. 0 removes the cap so tabs
# share the whole strip, browser-style. Default: 180.
tab-max-width = 180
# Horizontal gap between tabs; 0 makes them touch. Default: 6.
tab-gap = 6
# How far each tab floats inside the strip vertically; 0 gives flat,
# full-height tabs. Default: 7.
tab-inset-y = 7
# Corner rounding of each tab; 0 is square. Default: 6.
tab-radius = 6
# Fixed background for inactive / active tabs. Omit either and it
# adapts to your theme's background luminance. Default: adaptive.
tab-fill = "#2b2b2d"
tab-fill-active = "#4a4a4c"
# Hovering a tab shows its close button and a highlight; the close
# button then closes just that tab. false = classic stock behavior,
# close button on the active tab only. Default: true.
tab-close-on-hover = true
# Misclick guard for closing tabs: "never" closes on one action, "ask"
# pops a yes/no prompt, "double-click" arms the close button (it turns
# red) and a second action within a few seconds closes it. Applies to
# the close button, keyboard shortcuts and the command palette.
# Default: "never".
tab-close-confirm = "double-click"
```

Hover highlights, per-tab close buttons, a macOS-Terminal look or a flat
classic strip — your choice, not the theme's. And no more tabs lost to a
stray click: `tab-close-confirm` can require a yes/no prompt (`ask`) or a
deliberate second action — the close button arms and turns red, act again to
actually close (`double-click`).

### ⌨️ tmux-style keyboard split resizing

The `movedivider*` actions were rebuilt to behave like `tmux resize-pane`:

```toml
[bindings]
keys = [
  { key = "up",    with = "alt | shift", action = "movedividerup" },
  { key = "down",  with = "alt | shift", action = "movedividerdown" },
  { key = "left",  with = "alt | shift", action = "movedividerleft" },
  { key = "right", with = "alt | shift", action = "movedividerright" },
]
```

- **Directions are literal** — `left` always moves the divider left,
  regardless of which pane has focus.
- **One grid cell per press** — exact, reversible steps instead of fixed
  pixel jumps.
- **Deterministic divider ownership** — each key acts on the focused pane's
  bottom/right edge, so every divider in a stack is reachable by focusing the
  pane above or left of it.
- **Stable in mixed layouts** — nested vertical + horizontal splits resize at
  the correct container level, and the resulting flex weights round-trip
  through session save without drifting.

### 🛡️ Built perf-first and safety-conscious

Every feature above sits on Rio's per-line damage tracking, so none of it
costs a frame you weren't already paying for. That's not the whole bar,
though — this fork treats correctness under adversarial input as a
first-class requirement, not an afterthought:

- The session daemon caps its buffers, so a runaway pane can't turn into an
  unbounded memory sink.
- `rio-ptyd list` output is sanitized before it's rendered or parsed, so a
  hostile process name or path can't smuggle control sequences or corrupt
  the picker.
- Daemon signaling verifies the target pid before delivering a signal, so a
  stale or reused pid can't be killed by mistake.
- Session restore bounds the size of the file it reads and refuses to let a
  saved dump inject replies back into a live shell.
- Persistent panes are never silently orphaned or leaked — closing, quitting
  and `gc` all have well-defined, tested ownership rules.
- Triggers can't feed back into themselves in a loop, and coprocesses are
  reaped, not left running.
- Graphics evict GPU memory instead of accumulating it across a long
  session.

None of this is a pile of hacks bolted onto a demo. It's built to run
unattended, for days, against output you don't fully control.

### 🔧 Quality-of-life fixes

- Key bindings and `window.decorations` hot-reload on config save — no
  restart needed.
- Function-key and Enter bindings parse correctly in `[bindings]`.
- Bindable `togglemaximized` action.
- Middle-click paste from the primary selection buffer.
- Pixel-snapped, crisp HiDPI UI glyphs.
- Immediate repaint on tab close and on tab-bar show/hide — no stale frame
  hanging around.

## 📦 Build & Install

This fork is not published to any package channel — build it from source.
`dev` is the default branch and the product. MSRV is Rust 1.96.1.

```sh
git clone https://github.com/cantona/rio.git
cd rio
```

A bare `cargo build --release` builds every binary the fork needs — the
`rio` terminal and the `rio-ptyd` session daemon (`[session] persistent`
looks for it next to `rio`, then on `PATH`).

**Debian / Ubuntu** — a real package (binaries + terminfo + desktop entry +
icon), built and installed with `cargo deb`:

```sh
make install-debian-wayland   # or: make install-debian-x11
# build the .deb without installing (lands in release/debian/):
make release-debian-wayland
```

**Other Linux** — build, then install the binaries and runtime assets by
hand (this is what the .deb bundles):

```sh
make release-wayland          # or: make release-x11
install -Dm755 target/release/rio       /usr/local/bin/rio
install -Dm755 target/release/rio-ptyd  /usr/local/bin/rio-ptyd
sudo tic -xe xterm-rio,rio /usr/share/terminfo misc/rio.terminfo
install -Dm644 misc/rio.desktop /usr/share/applications/rio.desktop
install -Dm644 misc/logo.svg    /usr/share/icons/hicolor/scalable/apps/rio.svg
```

**macOS** — build the universal `Rio.app` (locally code-signed with an
ad-hoc identity) and move it into `/Applications`:

```sh
make install-macos
```

**Windows** — build an installer with
[`cargo-wix`](https://github.com/volks73/cargo-wix):

```sh
make release-windows          # produces an .msi
```

## ⚙️ Configuration

All base configuration is documented at
[rioterm.com/docs/config](https://rioterm.com/docs/config). The fork's
additions live in `[session]`, `[navigation]` and `triggers.toml` — every
option defaults to stock behavior, so an existing config keeps working
unchanged. This fork tracks upstream `main` and rebases regularly: you get
everything upstream ships, plus everything above, and every enhancement is
one config line to tune or turn off.

## 🙏 Credits

Rio is created and maintained by
[Raphael Amorim](https://github.com/raphamorim) — if you use Rio, consider
[sponsoring the original project](https://github.com/sponsors/raphamorim).
This fork exists to ship features while they wait for upstream review;
several are also submitted as upstream PRs.

MIT licensed, same as upstream.
