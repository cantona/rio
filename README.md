<!-- LOGO -->
<h1>
<p align="center">
  <img src="https://rioterm.com/assets/rio-logo.png" alt="Rio terminal logo" width="128">
  <br>Rio Terminal — cantona edition
</h1>
  <p align="center">
    A highly optimized, highly customizable fork of the GPU-accelerated
    Rio terminal — session restore, output-driven automation, working
    inline images, and a tab bar that bends to your config, not the
    other way around.
    <br />
    <a href="#why-this-fork">Why this fork</a>
    ·
    <a href="#highlights">Highlights</a>
    ·
    <a href="#build--install">Build & Install</a>
    ·
    <a href="#configuration">Configuration</a>
  </p>
</p>

A maintained fork of [raphamorim/rio](https://github.com/raphamorim/rio) that keeps
Rio's GPU-fast, damage-tracked rendering core and ships the features power users keep
asking for — implemented perf-first, so none of them cost you frames. Tracks upstream
`main` and rebases regularly: everything upstream has, plus everything below. The
`dev` branch is the product.

## Why this fork

This fork turns Rio's blazing GPU core into a complete daily driver:

- **Your workspace survives restarts.** Tabs, splits, working directories,
  window size and styled scrollback come back exactly as you left them —
  including named workspaces you can switch between.
- **Repetitive console work automates itself.** Regex-driven triggers watch
  the output and highlight, notify, recolor the tab, or type responses for
  you — a serial-console login becomes a zero-touch event.
- **Graphics just work.** `imgcat`, `chafa`, `viu`, sixel — inline images
  render correctly and stay solid under heavy use.
- **The interface is yours to shape.** The tab strip alone exposes ten
  config options — geometry, fills, hover behavior, close protection —
  every one applied live the moment you save.

All of it is built perf-first on top of Rio's per-line damage tracking, so
none of it costs you frames. Existing configs keep working unchanged, and
every enhancement is one config line to tune or turn off.

## Highlights

### 🔄 Session save & restore
Close the terminal, reopen it, and your world comes back — like a browser
restoring tabs. Every tab, the split layout with its ratios, each pane's
working directory, the window size, even the styled scrollback text.

Two levels control it. `restore` (level 1) decides *when* the session
is saved and reloaded; `persistent` (level 2) decides *how* — dead
scrollback or live shells.

```toml
[session]
# LEVEL 1 — when. "disable" turns sessions off entirely; "prompt" asks
# "save?" at quit and "resume?" at launch; "always" does both silently.
# Default: "disable".
restore = "prompt"
# How many scrollback lines each pane saves. Default: 2000.
max-scrollback-lines = 2000
```

- `ctrl+shift+s` saves on demand (with a "session saved" flash)
- Named workspaces: `rio --session work`, or the command palette's
  `Save Session As…` / `Restore Session…`
- With `persistent = false` (default), restored panes are fresh shells
  at the saved directory with the old output repainted above the prompt
- `"prompt"` asks at both ends and never writes without your yes;
  `"always"` saves silently and also autosaves on tab/split changes so a
  crash still leaves a current session
- The launch resume prompt offers three choices: `r` resume the saved
  session, `n` start new and discard the old one (its persistent
  daemons are killed, not stranded), or `k` start new but keep the old
  session in the background so a later launch can still resume it

### 🧬 Persistent shells that survive rio (tmux-style)
Turn on `persistent` and every pane runs behind a tiny standalone
daemon (`rio-ptyd`) that owns the real PTY. Quit rio — or let it crash —
and your shells keep running; the next launch reattaches to the live
processes and replays their screens, alt-screen apps (vim, `top`,
`minicom`) included. Closing a tab still kills its shell; only quitting
detaches.

```toml
[session]
# persistent only takes effect when restore is on (prompt/always);
# with restore = "disable" it is ignored.
restore = "prompt"
# Level 2 — false (default) = v1 scrollback repaint; true = v2 live
# rio-ptyd daemons so the shell outlives rio.
persistent = true
# Per-pane replay buffer, in bytes, that repaints the screen on
# reattach. Default: 1048576 (1 MiB).
persistent-ring-bytes = 1048576
```

`rio-ptyd` is a self-contained binary (no rio dependencies) you can
drive by hand — `rio-ptyd list`, `attach <id>`, `kill <id>`, `gc` — so a
session is inspectable and scriptable from any terminal. `list` shows
each pane's session name and what it is doing — the foreground program
when one is running (`vim`, `ssh`, …), otherwise its working directory:

```
$ rio-ptyd list
8bd31f72…  running   pid 40231  [work]  vim
ca203ac5…  running   pid 40988  [work]  /home/you/src
```

**Remote sessions over SSH.** Because the daemon speaks a byte protocol
rather than passing file descriptors, a pane hosted on another machine
attaches exactly like a local one. The command palette's
`Attach Remote Pane…` prompts for an `user@host`, lists that host's live
panes over `ssh … rio-ptyd list --json`, and opens the pick as a new
tab — repaint and all. Remote tabs are saved with their host and
reattach over SSH on the next restore. Closing a remote tab kills its
shell like any other; what rio never does implicitly is reap a remote
shell when you *decline* a session save or when `gc` cleans up — those
touch local daemons only.

### ⚡ Terminal triggers
iTerm2-style regex → action rules in a hot-reloading `triggers.toml`.
Six actions: highlight matches with a per-rule color, recolor the tab,
fire desktop notifications with urgency levels, run a command, pipe the
screen to a coprocess, or type text back into the terminal — plus
`once` (one-shot) and `instant` (fire mid-line) flags:

```toml
# In triggers.toml the rules are top-level `[[rules]]` tables — the
# file is parsed as the triggers config directly, not nested under a
# `[triggers]` key (that key only exists inside the main config).
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

Enough to script a full serial-console auto-login — `minicom` → detect
prompt → send credentials — or turn any log pattern into a desktop alert.

### 🖼️ Inline images that work
Sixel and iTerm2 (OSC 1337) graphics render correctly — `imgcat`,
`chafa -f iterm`, `chafa -f sixels`, `viu`, TIFF payloads included — and
stay robust across repeated large images. (Broken upstream:
[#1591](https://github.com/raphamorim/rio/issues/1591).)

### 🎨 A tab bar you control
Every dimension of the tab strip is a `[navigation]` config value, applied
live on save:

```toml
[navigation]
# Height of the tab strip, logical px. Default: 34.
tab-bar-height = 24
# Tab title font size, logical px. Default: 12.
tab-font-size = 14
# Widest a single tab may grow, logical px; 0 removes the cap so tabs
# share the whole strip like a browser. Default: 180.
tab-max-width = 0
# Horizontal space between tabs; 0 makes them touch. Default: 6.
tab-gap = 1
# How far each tab floats inside the strip vertically; 0 gives flat
# full-height tabs. Default: 7.
tab-inset-y = 0
# Corner rounding of each tab; 0 is square. Default: 6.
tab-radius = 6
# Fixed background for inactive / active tabs. Omit either and it
# adapts to your theme's background luminance. Default: adaptive.
tab-fill = "#2b2b2d"
tab-fill-active = "#4a4a4c"
# Hovering a tab shows its close button and a highlight, and the ×
# closes that tab directly. false = × on the active tab only.
# Default: true.
tab-close-on-hover = true
# Misclick protection for closing tabs: "never" closes on one action,
# "ask" pops a yes/no prompt, "double-click" arms the × (it turns red)
# and a second action within 3s closes. Applies to the close button,
# keyboard shortcuts and the command palette. Default: "never".
tab-close-confirm = "double-click"
```

Hover highlights, per-tab close buttons, macOS-Terminal or classic-flat
looks — your choice, not the theme's. And no more tabs lost to a stray
click: `tab-close-confirm` can require a y/n confirmation (`ask`) or a
deliberate second click — the × arms and turns red, click again to
close (`double-click`).

### ⌨️ tmux-style keyboard split resizing
The `movedivider*` actions were rebuilt from the ground up — they now
behave like `tmux resize-pane`:

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
- **One grid cell per press** — precise steps that scale with your
  font size, not fixed pixel jumps.
- **Deterministic divider ownership** — each key acts on the focused
  pane's bottom/right edge (the last pane in an axis falls back to its
  top/left edge), so in any stack every divider is reachable by
  focusing the pane above or left of it.
- **Stable in mixed layouts** — nested vertical + horizontal splits
  resize at the correct container level instead of corrupting the
  layout weights; a stack bordering the moved divider shares the
  change proportionally, keeping its internal balance.

### 🔧 Quality of life
Key bindings, `window.decorations` and more hot-reload on config save ·
middle-click paste from selection · crisper UI text on HiDPI · correct
grid resize when the tab bar appears · instant repaint when a shell
exits and its tab closes · `togglemaximized` action · function-key and
enter bindings parse correctly in `[bindings]`.

## Build & Install

This fork is not published to any package channel — build it from source.
`dev` is the default branch and the product. MSRV is Rust 1.96.1.

```sh
git clone https://github.com/cantona/rio.git
cd rio
```

A bare `cargo build --release` builds both binaries the fork needs — the
`rio` terminal and the `rio-ptyd` session daemon (`[session] persistent`
looks for it next to `rio`, then on `PATH`).

**Debian / Ubuntu** — a real package (binaries + terminfo + desktop
entry + icon), built and installed with `cargo deb`:

```sh
make install-debian-wayland   # or: make install-debian-x11
# build the .deb without installing (lands in release/debian/):
make release-debian-wayland
```

**Other Linux** — build, then install the binaries and the runtime
assets by hand (this is what the .deb bundles):

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

## Configuration

All base configuration is documented at
[rioterm.com/docs/config](https://rioterm.com/docs/config). The fork's
additions live in `[session]`, `[navigation]` and `triggers.toml` — every
option defaults to upstream behavior, so an existing config keeps working
unchanged.

## Credits

Rio is created and maintained by
[Raphael Amorim](https://github.com/raphamorim) — if you use Rio, consider
[sponsoring the original project](https://github.com/sponsors/raphamorim).
This fork exists to ship features while they wait for upstream review;
several are also submitted as upstream PRs.

MIT licensed, same as upstream.
