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

```toml
[session]
# What happens at quit/launch: "never" disables sessions entirely,
# "prompt" asks before saving and before resuming, "always" does both
# silently. Default: "never".
restore = "prompt"
# How many scrollback lines each pane saves. Default: 2000.
max-scrollback-lines = 2000
```

- `ctrl+shift+s` saves on demand (with a "session saved" flash)
- Named workspaces: `rio --session work`, or the command palette's
  `Save Session As…` / `Restore Session…`
- Restored panes are fresh shells at the saved directory with the old
  output repainted above the prompt

### ⚡ Terminal triggers
iTerm2-style regex → action rules in a hot-reloading `triggers.toml`.
Six actions: highlight matches with a per-rule color, recolor the tab,
fire desktop notifications with urgency levels, run a command, pipe the
screen to a coprocess, or type text back into the terminal — plus
`once` (one-shot) and `instant` (fire mid-line) flags:

```toml
[[triggers.rules]]
regex = "login:"
once = true            # one-shot; re-arm with the resettriggers action
[triggers.rules.action]
send_text = { text = "admin\n" }

[[triggers.rules]]
regex = "error: (.*)"
[triggers.rules.action]
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

```sh
git clone https://github.com/cantona/rio.git
cd rio
cargo build --release -p rioterm --no-default-features --features=wayland   # or x11
install -Dm755 target/release/rio ~/.local/bin/rio
```

MSRV is Rust 1.96.1. For macOS/Windows builds and base-feature docs, the
upstream documentation at [rioterm.com](https://rioterm.com) applies.

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
