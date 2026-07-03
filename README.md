<!-- LOGO -->
<h1>
<p align="center">
  <img src="https://rioterm.com/assets/rio-logo.png" alt="Rio terminal logo" width="128">
  <br>Rio Terminal — cantona edition
</h1>
  <p align="center">
    The GPU-accelerated Rio terminal, with sessions, automation triggers,
    working inline images and a fully customizable tab bar.
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

A maintained fork of [raphamorim/rio](https://github.com/raphamorim/rio) that ships
the features power users keep asking for. Tracks upstream `main` and rebases
regularly — everything upstream has, plus everything below. The `dev` branch is the
product.

## Why this fork

Rio is a fantastic, fast terminal — but some workflows need more than fast:
picking up your work exactly where you left it, automating repetitive
console interactions, and actually seeing the images your tools print.
This fork adds those, upstreamable-quality and CI-green, without changing
any default behavior: with a stock config it behaves like stock Rio.

## Highlights

### 🔄 Session save & restore
Close the terminal, reopen it, and your world comes back — like a browser
restoring tabs. Every tab, the split layout with its ratios, each pane's
working directory, even the styled scrollback text.

```toml
[session]
restore = "prompt"   # never | prompt | always
```

- `ctrl+shift+s` saves on demand (with a "session saved" flash)
- Named workspaces: `rio --session work`, or the command palette's
  `Save Session As…` / `Restore Session…`
- Restored panes are fresh shells at the saved directory with the old
  output repainted above the prompt

### ⚡ Terminal triggers
iTerm2-style regex → action rules in a hot-reloading `triggers.toml`.
Highlight matches, fire desktop notifications with urgency levels, run
commands, or send text back to the terminal — enough to script a full
serial-console auto-login (`minicom` → detect prompt → send credentials)
with one-shot rules and an `alt+r` re-arm.

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
tab-bar-height = 24        # strip height (default 34)
tab-font-size = 14         # title size (default 12)
tab-max-width = 0          # 0 = tabs expand to fill the strip
tab-gap = 1                # px between tabs, 0 = touching
tab-inset-y = 0            # 0 = flat full-height tabs
tab-radius = 6             # corner rounding, 0 = square
tab-fill = "#2b2b2d"       # fixed fills, or omit for adaptive
tab-fill-active = "#4a4a4c"
tab-close-on-hover = true  # Terminal.app-style ×-on-hover, closes any tab
```

Hover highlights, per-tab close buttons, macOS-Terminal or classic-flat
looks — your choice, not the theme's.

### 🔧 Quality of life
Key bindings, `window.decorations` and more hot-reload on config save ·
middle-click paste from selection · crisper UI text on HiDPI · correct
grid resize when the tab bar appears · `togglemaximized` action ·
function-key and enter bindings parse correctly in `[bindings]`.

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
