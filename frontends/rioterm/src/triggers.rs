use crate::hints::extract_line_text;
use rio_backend::config::triggers::{TriggerAction, Triggers as TriggersConfig};
use rio_backend::crosswords::grid::Dimensions;
use rio_backend::crosswords::pos::{Column, Line, Pos};
use rio_backend::crosswords::search::Match;
use rio_backend::crosswords::Crosswords;
use rio_backend::event::{EventListener, TerminalDamage};
use rustc_hash::{FxHashMap, FxHashSet};
use std::hash::{Hash, Hasher};

/// Longest line (chars) matched against trigger regexes.
const LINE_SCAN_CAP: usize = 4096;

/// Scrollback lines (above the visible bottom) included when a feed_screen
/// coprocess captures the screen, so a multi-line block that scrolled partly
/// off the top is still captured whole.
const FEED_HISTORY_LINES: i32 = 200;

/// Upper bound (bytes) on a feed_screen payload. The consumer writes stdin
/// before draining stdout, so a payload larger than the OS pipe buffer (~64KB
/// on Linux) could deadlock; keep the capture comfortably under it. Truncated
/// at a char boundary from the newest (bottom) end so the visible prompt is
/// always kept.
const FEED_PAYLOAD_CAP: usize = 48 * 1024;

struct CompiledTrigger {
    regex: onig::Regex,
    instant: bool,
    once: bool,
    action: TriggerAction,
    /// Stable identity (regex + action), independent of the rule's index in
    /// the list, so `once` dedup survives a config reload that inserts or
    /// reorders rules (an index would then point at a different rule).
    id: u64,
}

/// The cursor (live prompt) line's fire state for one route: the row the
/// cursor last sat on, and which (rule, match text) pairs already fired there.
/// The set is cleared only when the cursor ROW NUMBER changes, so an action
/// whose output echoes back into the same line (`send_text "y"` -> `[y/n]y`)
/// does not re-fire the rule, yet a fresh prompt drawn at a new row does.
#[derive(Default)]
struct CursorFired {
    /// The cursor's ABSOLUTE line (history + screen row) when these matches
    /// fired — not the screen row, which stays constant on a scrolling
    /// console and would suppress every re-drawn prompt as an echo.
    row: i64,
    fired: FxHashSet<(u64, u64)>,
}

/// Compiled trigger rules plus per-route dedup. Owned on the main thread
/// (`onig::Regex` is `!Send`).
#[derive(Default)]
pub struct Triggers {
    rules: Vec<CompiledTrigger>,
    has_highlight: bool,
    /// Any rule pipes the screen to a coprocess; gate the screen capture.
    has_feed_screen: bool,
    /// Per route, the set of (absolute line, content hash, finalized) we've
    /// already evaluated, so a given line+content fires once. Keyed on
    /// content rather than a cursor counter so prompt redraws and TUIs
    /// (which don't scroll) still register new output.
    seen: FxHashMap<usize, FxHashSet<(i64, u64, bool)>>,
    /// (route, stable rule id) of `once` rules that have already fired.
    /// Retained across rebuild (config reload) — a stable id keeps the match
    /// correct even when the rule list changes.
    fired_once: FxHashSet<(usize, u64)>,
    /// Per route, the cursor line's fire state (row + fired rule/match pairs).
    /// A rule fires again on the cursor line only when a genuinely new match
    /// appears; an echo of an action's own output, or the line merely growing
    /// by a chunk, does not re-fire. See `CursorFired`.
    cursor_fired: FxHashMap<usize, CursorFired>,
    /// Per route, the last (screen_lines, columns) seen. A change means a
    /// resize/font-zoom reflowed the grid, rewriting absolute line numbers and
    /// wrapping, which invalidates `seen`'s (abs_line, content) keys; the next
    /// scan then re-seeds `seen` without firing so reflow doesn't mass re-fire.
    dims: FxHashMap<usize, (usize, usize)>,
}

/// A one-shot trigger action with captures already substituted.
pub enum ResolvedAction {
    Notify {
        title: String,
        body: String,
        urgency: u8,
    },
    TabColor([f32; 4]),
    Run {
        program: String,
        args: Vec<String>,
    },
    SendText(String),
    Coprocess {
        program: String,
        args: Vec<String>,
        stdin: Option<String>,
    },
}

#[inline]
fn rgba_u8(c: [f32; 4]) -> [u8; 4] {
    [
        (c[0] * 255.0).round() as u8,
        (c[1] * 255.0).round() as u8,
        (c[2] * 255.0).round() as u8,
        (c[3] * 255.0).round() as u8,
    ]
}

#[inline]
fn hash_text(s: &str) -> u64 {
    let mut h = rustc_hash::FxHasher::default();
    s.hash(&mut h);
    h.finish()
}

/// Stable identity for a rule: its regex plus a textual rendering of its
/// action. Unlike the rule's list index, this survives inserting/reordering
/// rules across a config reload, so `once` dedup stays attached to the same
/// rule rather than being re-armed or leaking onto a different one.
fn rule_id(regex: &str, action: &TriggerAction) -> u64 {
    let mut h = rustc_hash::FxHasher::default();
    regex.hash(&mut h);
    format!("{action:?}").hash(&mut h);
    h.finish()
}

/// Hash of the matched substring (whole match), used to dedup cursor-line
/// fires by what matched rather than by the whole line's content, so an echo
/// or a mid-line chunk append that reproduces an already-fired match is
/// suppressed while a genuinely new match still fires.
#[inline]
fn match_hash(text: &str, caps: &onig::Captures) -> u64 {
    match caps.pos(0) {
        Some((s, e)) => hash_text(&text[s..e]),
        None => 0,
    }
}

impl Triggers {
    pub fn new(config: &TriggersConfig) -> Self {
        let mut rules = Vec::with_capacity(config.rules.len());
        for rule in &config.rules {
            match onig::Regex::new(&rule.regex) {
                Ok(regex) => rules.push(CompiledTrigger {
                    regex,
                    instant: rule.instant,
                    once: rule.once,
                    id: rule_id(&rule.regex, &rule.action),
                    action: rule.action.clone(),
                }),
                Err(err) => {
                    tracing::warn!("invalid trigger regex {:?}: {}", rule.regex, err);
                }
            }
        }
        let has_highlight = rules
            .iter()
            .any(|r| matches!(r.action, TriggerAction::Highlight { .. }));
        let has_feed_screen = rules.iter().any(|r| {
            matches!(
                r.action,
                TriggerAction::Coprocess {
                    feed_screen: true,
                    ..
                }
            )
        });
        Self {
            rules,
            has_highlight,
            has_feed_screen,
            seen: FxHashMap::default(),
            fired_once: FxHashSet::default(),
            cursor_fired: FxHashMap::default(),
            dims: FxHashMap::default(),
        }
    }

    /// Recompile rules from config (config hot-reload), PRESERVING the
    /// per-route dedup state. `*self = Triggers::new(...)` would wipe
    /// `seen`/`fired_once`, so the next scan would treat every line
    /// already on screen as new — re-firing `once` rules and re-running
    /// `send_text` (e.g. re-typing a saved credential) just because an
    /// unrelated config file was edited. Only the rules change here.
    pub fn rebuild(&mut self, config: &TriggersConfig) {
        let fresh = Triggers::new(config);
        self.rules = fresh.rules;
        self.has_highlight = fresh.has_highlight;
        self.has_feed_screen = fresh.has_feed_screen;
        // seen / fired_once / cursor_fired / dims intentionally retained;
        // fired_once is keyed on stable rule ids so retention stays correct
        // even when the rule list is edited.
    }

    /// Forget a route's accumulated dedup state when its pane closes,
    /// so `seen`/`fired_once`/`cursor_fired`/`dims` don't grow without bound
    /// over a long session that opens and closes many tabs.
    pub fn forget_route(&mut self, route_id: usize) {
        self.seen.remove(&route_id);
        self.cursor_fired.remove(&route_id);
        self.dims.remove(&route_id);
        self.fired_once.retain(|(r, _)| *r != route_id);
    }

    /// Re-arm `once` rules so the automation can run again. Bound to
    /// `ResetTriggers` (e.g. Alt+R). Drops only the cursor-line (non-finalized)
    /// dedup, so instant rules (a `login:`/`Password:` prompt) re-fire on the
    /// current live line even when it sits where one fired before. Finalized
    /// (scrolled-past) content stays deduped, so a re-arm doesn't replay a
    /// whole stale flow at once.
    pub fn reset(&mut self) {
        self.fired_once.clear();
        for seen in self.seen.values_mut() {
            seen.retain(|(_, _, finalized)| *finalized);
        }
        self.cursor_fired.clear();
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Match new output against the one-shot rules and return resolved
    /// actions. Scans the live (non-scrolled) screen and dedups by
    /// (line, content) so each line fires once: lines above the cursor are
    /// "finalized" (non-instant rules); the cursor line fires instant rules.
    pub fn scan<T: EventListener>(
        &mut self,
        route_id: usize,
        term: &Crosswords<T>,
    ) -> Vec<ResolvedAction> {
        if self.rules.is_empty() {
            return Vec::new();
        }

        let grid = &term.grid;
        let history = grid.history_size() as i64;
        let cursor_row = grid.cursor.pos.row.0 as i64;
        let screen_lines = grid.screen_lines();
        let columns = term.columns();

        // A resize / font-zoom reflows the grid: absolute line numbers and
        // wrapping are rewritten, so `seen`'s (abs_line, content) keys no
        // longer identify the same output and every visible line would look
        // new. Detect it by a dimension change and re-seed `seen` from the
        // current screen WITHOUT firing, so already-visible finalized output
        // is suppressed while genuinely new output still fires.
        let reflowed = self.dims.insert(route_id, (screen_lines, columns))
            != Some((screen_lines, columns));

        // Skip when the terminal content did not change since the last render.
        // scan() runs at the top of every render() — cursor blink, mouse hover
        // and other UI-only repaints included — but only Full/Partial damage
        // means cells actually changed. Noop/CursorOnly frames do no scan work
        // (this walks and hashes every visible line under the terminal lock).
        // A reflow always reports Full damage, so the re-seed above is reached.
        match term.peek_damage_event() {
            Some(TerminalDamage::Full) | Some(TerminalDamage::Partial) => {}
            _ => return Vec::new(),
        }

        // Captured lazily on the first feed_screen match (see below) so the
        // common path — and every non-matching frame — pays nothing.
        let mut screen_text: Option<String> = None;

        let seen = self.seen.entry(route_id).or_default();
        if reflowed {
            seen.clear();
        }
        // Drop lines that have scrolled out of the live view.
        seen.retain(|(abs, _, _)| *abs >= history);
        // In the alternate screen history never advances, so the retain
        // above frees nothing and a redrawing TUI (htop/watch) grows the
        // set without bound. Cap it: clearing costs at most a re-fire of
        // whatever is currently on screen, which the per-line dedup then
        // re-suppresses immediately.
        if seen.len() > 8192 {
            seen.clear();
        }

        // Reset the cursor line's fire set when the cursor moved to a new
        // ABSOLUTE line, so a fresh prompt fires while an echo/growth on the
        // same line cannot. Keying on the screen row broke a scrolling
        // console (minicom on serial): the cursor sits on the bottom row
        // forever, so every re-drawn `login:` prompt reused the same
        // (row, match) key and was suppressed as an echo. The absolute line
        // (history + cursor_row) advances as the buffer scrolls, so a new
        // prompt scrolled to the same bottom row still gets a new key. A
        // scrolled-back view (offset != 0) is not the live prompt, so leave
        // the fire set untouched.
        let live = grid.display_offset() == 0;
        if live {
            let cursor_abs = history + cursor_row;
            let cf = self.cursor_fired.entry(route_id).or_default();
            if cf.row != cursor_abs {
                cf.row = cursor_abs;
                cf.fired.clear();
            }
        }

        let mut actions = Vec::new();
        for i in 0..screen_lines {
            let abs = history + i as i64;
            let is_cursor = live && (i as i64) == cursor_row;
            let finalized = (i as i64) < cursor_row;
            let text = extract_line_text(term, Line(i as i32));
            if text.is_empty() {
                continue;
            }
            let text: &str = if text.len() > LINE_SCAN_CAP {
                match text.char_indices().nth(LINE_SCAN_CAP) {
                    Some((byte, _)) => &text[..byte],
                    None => &text,
                }
            } else {
                &text
            };

            // The cursor (live prompt) line is handled per-match below so an
            // echo of an action's own output doesn't re-fire it (findings on
            // send_text/coprocess feedback and growing instant matches). Other
            // lines fire once each, keyed on (line, content, phase). When
            // re-seeding after a reflow, record the key but don't fire.
            if !is_cursor {
                let fresh = seen.insert((abs, hash_text(text), finalized));
                if !fresh || reflowed {
                    continue;
                }
            }

            for rule in &self.rules {
                if matches!(rule.action, TriggerAction::Highlight { .. }) {
                    continue;
                }
                // Finalized lines run non-instant rules; the cursor line
                // runs instant rules (prompts with no trailing newline).
                if rule.instant != is_cursor {
                    continue;
                }
                if rule.once && self.fired_once.contains(&(route_id, rule.id)) {
                    continue;
                }
                let mut matched = false;
                for caps in rule.regex.captures_iter(text) {
                    // On the cursor line, dedup by the matched substring so an
                    // action's echo (or the line growing by a chunk) that
                    // reproduces an already-fired match is suppressed, while a
                    // new match on the same row still fires.
                    if is_cursor {
                        let key = (rule.id, match_hash(text, &caps));
                        if !self
                            .cursor_fired
                            .get_mut(&route_id)
                            .expect("cursor_fired seeded above")
                            .fired
                            .insert(key)
                        {
                            continue;
                        }
                    }
                    if self.has_feed_screen
                        && screen_text.is_none()
                        && matches!(
                            rule.action,
                            TriggerAction::Coprocess {
                                feed_screen: true,
                                ..
                            }
                        )
                    {
                        screen_text = Some(capture_screen(term));
                    }
                    actions.push(resolve(&rule.action, &caps, screen_text.as_deref()));
                    matched = true;
                    // A `once` rule fires a single action per line even
                    // when the pattern occurs several times — otherwise
                    // it would emit N notifications/runs before
                    // fired_once is set after the loop.
                    if rule.once {
                        break;
                    }
                }
                if matched && rule.once {
                    self.fired_once.insert((route_id, rule.id));
                }
            }
        }
        actions
    }

    /// Recompute highlight ranges over the visible region, or `None` when the
    /// terminal content did not change since the last render so the caller
    /// should keep the highlights it already has. Highlights are a visual
    /// state, but re-running onig over every visible line each frame — under
    /// the terminal lock, on cursor-blink and hover repaints too — is wasted
    /// work when no cell changed. Content change (Full/Partial damage) forces
    /// a recompute; an empty `Vec` still means "clear", e.g. when the last
    /// matching text scrolled off or the highlight rules were removed.
    pub fn highlights<T: EventListener>(
        &self,
        term: &Crosswords<T>,
    ) -> Option<Vec<(Match, [u8; 4])>> {
        if !self.has_highlight {
            return Some(Vec::new());
        }
        match term.peek_damage_event() {
            Some(TerminalDamage::Full) | Some(TerminalDamage::Partial) => {}
            _ => return None,
        }
        let grid = &term.grid;
        let display_offset = grid.display_offset() as i32;
        let topmost = grid.topmost_line().0;
        let mut out = Vec::new();
        for i in 0..grid.screen_lines() {
            let line = Line(i as i32 - display_offset);
            if line.0 < topmost {
                continue;
            }
            let text = extract_line_text(term, line);
            if text.is_empty() {
                continue;
            }
            for rule in &self.rules {
                let TriggerAction::Highlight { color } = &rule.action else {
                    continue;
                };
                let rgba = rgba_u8(*color);
                // Cap matches per rule per line: a pathological pattern
                // on adversarial output could otherwise push an unbounded
                // number of ranges every frame (this runs under the
                // terminal lock, on the render path).
                for caps in rule.regex.captures_iter(&text).take(256) {
                    if let Some((start, end)) = span(&text, &caps) {
                        out.push((Pos::new(line, start)..=Pos::new(line, end), rgba));
                    }
                }
            }
        }
        Some(out)
    }
}

/// Visible screen plus recent scrollback as one newline-joined string, for a
/// feed_screen coprocess. Recent history is included so a multi-line block
/// that has scrolled partly above the visible area is still captured whole.
fn capture_screen<T: EventListener>(term: &Crosswords<T>) -> String {
    let grid = &term.grid;
    let screen_lines = grid.screen_lines() as i32;
    let start = grid.topmost_line().0.max(-FEED_HISTORY_LINES);
    let text = (start..screen_lines)
        .map(|i| extract_line_text(term, Line(i)))
        .collect::<Vec<_>>()
        .join("\n");
    if text.len() <= FEED_PAYLOAD_CAP {
        return text;
    }
    // Keep the newest end (visible prompt); drop the oldest scrollback.
    let cut = text.len() - FEED_PAYLOAD_CAP;
    match text.char_indices().find(|(i, _)| *i >= cut) {
        Some((byte, _)) => text[byte..].to_string(),
        None => text,
    }
}

/// Match span as cell columns (onig reports byte offsets; columns are
/// per-cell, one char each).
fn span(text: &str, caps: &onig::Captures) -> Option<(Column, Column)> {
    let (start_b, end_b) = caps.pos(0)?;
    let start = text[..start_b].chars().count();
    let end = text[..end_b].chars().count().saturating_sub(1);
    Some((Column(start), Column(end.max(start))))
}

fn resolve(
    action: &TriggerAction,
    caps: &onig::Captures,
    screen: Option<&str>,
) -> ResolvedAction {
    match action {
        TriggerAction::Notify {
            title,
            body,
            urgency,
        } => ResolvedAction::Notify {
            title: substitute(title, caps),
            body: substitute(body, caps),
            urgency: urgency.level(),
        },
        TriggerAction::TabColor { color } => ResolvedAction::TabColor(*color),
        TriggerAction::Run { program, args } => ResolvedAction::Run {
            program: program.clone(),
            args: args.iter().map(|a| substitute(a, caps)).collect(),
        },
        TriggerAction::SendText { text } => {
            ResolvedAction::SendText(substitute(text, caps))
        }
        TriggerAction::Coprocess {
            program,
            args,
            feed_screen,
        } => ResolvedAction::Coprocess {
            program: program.clone(),
            args: args.iter().map(|a| substitute(a, caps)).collect(),
            stdin: if *feed_screen {
                screen.map(str::to_owned)
            } else {
                None
            },
        },
        // Handled by `highlights()`; `scan` skips it.
        TriggerAction::Highlight { .. } => ResolvedAction::SendText(String::new()),
    }
}

/// Expand `\0..\9` (whole match / capture groups) and `\\` in `template`.
fn substitute(template: &str, caps: &onig::Captures) -> String {
    if !template.contains('\\') {
        return template.to_owned();
    }
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some(d) if d.is_ascii_digit() => {
                let n = (*d as u8 - b'0') as usize;
                chars.next();
                if let Some(group) = caps.at(n) {
                    out.push_str(group);
                }
            }
            Some('\\') => {
                out.push('\\');
                chars.next();
            }
            _ => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps<'a>(re: &str, text: &'a str) -> onig::Captures<'a> {
        onig::Regex::new(re).unwrap().captures(text).unwrap()
    }

    #[test]
    fn substitute_groups() {
        let c = caps(r"error: (\w+) (\w+)", "error: disk full");
        assert_eq!(substitute(r"\0", &c), "error: disk full");
        assert_eq!(substitute(r"\1/\2", &c), "disk/full");
        assert_eq!(substitute(r"\9", &c), "");
        assert_eq!(substitute(r"a\\b", &c), r"a\b");
        assert_eq!(substitute("plain", &c), "plain");
    }

    #[test]
    fn rule_id_is_stable_and_distinct() {
        let color = [0.0, 0.0, 0.0, 1.0];
        let a = TriggerAction::TabColor { color };
        let b = TriggerAction::SendText { text: "y\n".into() };
        // Same regex + same action -> same id across calls (survives reload).
        assert_eq!(rule_id("done", &a), rule_id("done", &a));
        // A different regex or a different action -> different id.
        assert_ne!(rule_id("done", &a), rule_id("finished", &a));
        assert_ne!(rule_id("done", &a), rule_id("done", &b));
    }

    #[test]
    fn match_hash_tracks_matched_substring() {
        let re = onig::Regex::new(r"\[y/n\]").unwrap();
        // The whole line grows as an echo appends, but the match text is the
        // same, so the cursor-line dedup key is unchanged -> no re-fire.
        let c1 = re.captures("Continue? [y/n]").unwrap();
        let c2 = re.captures("Continue? [y/n]y").unwrap();
        assert_eq!(
            match_hash("Continue? [y/n]", &c1),
            match_hash("Continue? [y/n]y", &c2)
        );
    }
}
