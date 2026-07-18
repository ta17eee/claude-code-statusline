//! Claude Code status line: two-line display with block/braille progress bars.
//! With `--subagent`, renders agent-panel task rows instead (subagentStatusLine
//! protocol: tasks in, JSON Lines out). See ../CLAUDE.md for design rationale.

use chrono::{Local, TimeZone};
use serde_json::Value;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const R: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[38;2;100;180;255m";

const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const BRAILLE: [char; 9] = [' ', '⡀', '⣀', '⣄', '⣤', '⣦', '⣶', '⣷', '⣿'];

const GIT_TIMEOUT: Duration = Duration::from_millis(500);

// ── ANSI helpers ──────────────────────────────────────────────────────────────

/// Green (low) → red (high) 24-bit colour escape.
fn gradient(pct: f64) -> String {
    if pct < 50.0 {
        let r = (pct * 5.1) as i64;
        format!("\x1b[38;2;{r};200;80m")
    } else {
        let g = (200.0 - (pct - 50.0) * 4.0).max(0.0) as i64;
        format!("\x1b[38;2;255;{g};60m")
    }
}

// ── Bar renderers ─────────────────────────────────────────────────────────────

fn render_bar(chars: &[char; 9], pct: f64, width: usize) -> String {
    let levels = chars.len() - 1;
    let pct = pct.clamp(0.0, 100.0);
    let level = pct / 100.0;
    let mut out = String::with_capacity(width * 3); // chars are up to 3 bytes in UTF-8
    for i in 0..width {
        let seg_start = i as f64 / width as f64;
        let seg_end = (i + 1) as f64 / width as f64;
        if level >= seg_end {
            out.push(chars[levels]);
        } else if level <= seg_start {
            out.push(chars[0]);
        } else {
            let frac = (level - seg_start) / (seg_end - seg_start);
            let idx = ((frac * levels as f64) as usize + 1).min(levels);
            out.push(chars[idx]);
        }
    }
    out
}

fn block_bar(pct: f64) -> String {
    render_bar(&BLOCKS, pct, 8)
}

fn braille_bar(pct: f64) -> String {
    render_bar(&BRAILLE, pct, 8)
}

fn fmt_metric(label: &str, pct: f64, bar: String, color_pct: Option<f64>) -> String {
    let p = pct.round_ties_even() as i64;
    let cpct = color_pct.unwrap_or(pct);
    format!("{DIM}{label}{R} {}{bar}{R} {p}%", gradient(cpct))
}

// ── Rate limit pace projection ───────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Window {
    FiveHour,
    SevenDay,
}

impl Window {
    fn label(self) -> &'static str {
        match self {
            Window::FiveHour => "5h",
            Window::SevenDay => "7d",
        }
    }
    fn seconds(self) -> f64 {
        match self {
            Window::FiveHour => 5.0 * 3600.0,
            Window::SevenDay => 7.0 * 86400.0,
        }
    }
    // Bootstrap fraction per window: longer windows (7d) need a longer bootstrap
    // because human work patterns (sleep, weekends) make early pace projection
    // systematically over-estimate compared to 5h, where usage is roughly continuous.
    fn pace_threshold(self) -> f64 {
        match self {
            Window::FiveHour => 0.10,
            Window::SevenDay => 0.30,
        }
    }
}

fn fmt_rate_limit(limit_data: &Value, window: Window, now: f64) -> Option<String> {
    let used_pct = limit_data.get("used_percentage")?.as_f64()?;
    let resets_at = limit_data.get("resets_at").and_then(|v| v.as_f64());
    let label = window.label();

    let Some(resets_at) = resets_at else {
        return Some(fmt_metric(label, used_pct, braille_bar(used_pct), None));
    };

    let window_secs = window.seconds();
    let elapsed = now - (resets_at - window_secs);
    let elapsed_ratio = elapsed / window_secs;

    // Floor effective elapsed at the bootstrap fraction so the projection stays
    // bounded in early window without a color discontinuity at the boundary.
    let threshold = window.pace_threshold();
    let effective_elapsed = elapsed.max(window_secs * threshold);
    let projected = used_pct * window_secs / effective_elapsed;
    let progress = ((elapsed_ratio - threshold) / (1.0 - threshold)).max(0.0);

    let color_pct = if projected <= 80.0 {
        0.0
    } else if projected <= 95.0 {
        (projected - 80.0) / 15.0 * 40.0
    } else {
        let red_at = 100.0 + 50.0 * (1.0 - progress); // 150 (early) → 100 (late)
        40.0 + ((projected - 95.0) / (red_at - 95.0)).min(1.0) * 60.0
    };

    let mut result = fmt_metric(label, used_pct, braille_bar(used_pct), Some(color_pct));

    let show_reset = matches!(window, Window::FiveHour) || projected >= 90.0;
    if show_reset {
        let remaining = resets_at - now;
        if let Some(dt) = Local.timestamp_opt(resets_at as i64, 0).single() {
            // %b/%-d are always English/unpadded regardless of system locale (chrono
            // only localizes with the separate "unstable-locales" feature, which is
            // not enabled), matching the Python version's locale-independent format.
            let reset_str = if remaining >= 86400.0 {
                dt.format("%b%-d").to_string()
            } else {
                dt.format("%H:%M").to_string()
            };
            result.push_str(&format!(" {DIM}@{reset_str}{R}"));
        }
    }

    Some(result)
}

// ── Rate limit cache (cross-session) ────────────────────────────────────────

/// `None` if the home directory can't be determined — callers should skip the
/// cache entirely rather than fall back to a cwd-relative path.
fn cache_path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".claude").join("statusline-cache.json"))
}

fn read_cache(path: Option<&Path>, now: f64) -> Value {
    let empty = || Value::Object(serde_json::Map::new());
    let Some(path) = path else {
        return empty();
    };
    let Ok(content) = std::fs::read_to_string(path) else {
        return empty();
    };
    let Ok(v) = serde_json::from_str::<Value>(&content) else {
        return empty();
    };
    let ts = v.get("ts").and_then(|t| t.as_f64()).unwrap_or(0.0);
    if now - ts > 7.0 * 86400.0 {
        return empty();
    }
    v
}

fn write_cache(path: Option<&Path>, limits: &Value, now: f64) {
    let Some(path) = path else {
        return;
    };
    // Unique per process so concurrent sessions don't interleave writes to the
    // same tmp file; the final rename is still atomic regardless of which wins.
    let tmp = PathBuf::from(format!("{}.{}.tmp", path.display(), std::process::id()));

    let mut obj = limits.as_object().cloned().unwrap_or_default();
    obj.insert("ts".to_string(), serde_json::json!(now));
    let Ok(content) = serde_json::to_string(&Value::Object(obj)) else {
        return;
    };
    if std::fs::write(&tmp, content).is_err() {
        return;
    }
    let _ = std::fs::rename(&tmp, path);
}

/// Return entry as-is, or reset to 0% if its window has already elapsed.
fn expire(entry: &Value, now: f64) -> Value {
    if let Some(resets_at) = entry.get("resets_at").and_then(|v| v.as_f64()) {
        if resets_at < now {
            return serde_json::json!({"used_percentage": 0});
        }
    }
    entry.clone()
}

/// Pick the fresher rate-limit entry for one window.
fn pick_fresher(a: &Value, b: &Value, now: f64) -> Value {
    let a = expire(a, now);
    let b = expire(b, now);
    let ar = a.get("resets_at").and_then(|v| v.as_f64());
    let br = b.get("resets_at").and_then(|v| v.as_f64());
    match (ar, br) {
        (None, None) => {
            if a.get("used_percentage").is_some() {
                a
            } else {
                b
            }
        }
        (None, Some(_)) => b,
        (Some(_), None) => a,
        (Some(ar), Some(br)) => {
            if ar != br {
                // Both non-expired but disagree on resets_at: an account is only ever
                // in one window at a time, so this shouldn't happen under correct
                // operation. Trust live data rather than the larger resets_at —
                // otherwise a single corrupted cache write poisons every session
                // until wall-clock time passes the bad value (ts refreshes on every
                // write, so the 7-day staleness cutoff never kicks in to save us).
                a
            } else {
                let au = a.get("used_percentage").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let bu = b.get("used_percentage").and_then(|v| v.as_f64()).unwrap_or(0.0);
                if au >= bu {
                    a
                } else {
                    b
                }
            }
        }
    }
}

// ── Git info (gix, run on a watchdog thread) ─────────────────────────────────

#[derive(Default)]
struct GitInfo {
    toplevel: String,
    commit_hash: String,
    branch: String,
}

fn discover_git_info(dir: &str) -> Option<GitInfo> {
    let repo = gix::discover(dir).ok()?;
    let toplevel = repo.workdir()?.to_string_lossy().to_string();

    let mut branch = String::new();
    let mut commit_hash = String::new();
    if let Ok(head) = repo.head() {
        use gix::head::Kind;
        match &head.kind {
            Kind::Symbolic(r) => branch = r.name.shorten().to_string(),
            Kind::Unborn(name) => branch = name.shorten().to_string(),
            Kind::Detached { .. } => {}
        }
        // head.id() derives straight from the already-fetched `kind` (no extra ref
        // resolution), unlike repo.head_id() which re-resolves HEAD from scratch.
        if let Some(id) = head.id() {
            commit_hash = id.shorten_or_id().to_string();
        }
    }

    Some(GitInfo { toplevel, commit_hash, branch })
}

/// Start the git lookup on a background thread immediately so it overlaps with
/// line-1 work; collect it (with a timeout) right before building line 2.
fn spawn_git_lookup(dir: &str) -> mpsc::Receiver<Option<GitInfo>> {
    let dir = dir.to_string();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(discover_git_info(&dir));
    });
    rx
}

fn collect_git_info(rx: mpsc::Receiver<Option<GitInfo>>) -> GitInfo {
    rx.recv_timeout(GIT_TIMEOUT).ok().flatten().unwrap_or_default()
}

/// Path from `base` to `target`, mirroring Python's `os.path.relpath(target, base)`.
fn relpath(target: &Path, base: &Path) -> String {
    let target_components: Vec<_> = target.components().collect();
    let base_components: Vec<_> = base.components().collect();
    let common_len = target_components
        .iter()
        .zip(base_components.iter())
        .take_while(|(a, b)| a == b)
        .count();

    let mut parts: Vec<String> = Vec::new();
    for _ in common_len..base_components.len() {
        parts.push("..".to_string());
    }
    for c in &target_components[common_len..] {
        parts.push(c.as_os_str().to_string_lossy().to_string());
    }

    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

/// Mirror Python's `re.sub(r'\s*\([^)]*context\)', '', model)`: strip any
/// parenthetical ending in "context", along with whitespace immediately before it.
fn strip_context_suffix(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut result = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '(' {
            if let Some(close_offset) = chars[i + 1..].iter().position(|&c| c == ')') {
                let close_idx = i + 1 + close_offset;
                let inner: String = chars[i + 1..close_idx].iter().collect();
                if inner.ends_with("context") {
                    while matches!(result.chars().last(), Some(c) if c.is_whitespace()) {
                        result.pop();
                    }
                    i = close_idx + 1;
                    continue;
                }
            }
        }
        result.push(chars[i]);
        i += 1;
    }
    result
}

// ── Subagent status line (--subagent) ────────────────────────────────────────
//
// Protocol: one invocation receives every running panel task; stdout is JSON
// Lines, one {"id", "content"} object per overridden row. `content` replaces
// the whole row body after the status bullet, so identity/summary/metrics must
// all be re-emitted here. Tasks we print nothing for keep Claude Code's default
// rendering — that fail-open path is the error handling strategy throughout.
//
// Output is deliberately plain text (no ANSI): the host wraps the row in its
// own dim/bold styling per focus state, and embedded escape codes would break
// that. Number/duration formats mirror the host's formatters so overridden and
// default rows read identically.

const NAME_COL_MIN: usize = 4;
const NAME_COL_MAX: usize = 28; // host caps its name column at 28 too
const NAME_SUMMARY_GAP: usize = 2;
const RIGHT_MIN_GAP: usize = 1;
const RAW_MODEL_MAX: usize = 20;

fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Truncate to `max` display columns, appending `…` when anything was cut.
fn truncate_display(s: &str, max: usize) -> String {
    if display_width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > max - 1 {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// Mirror the host's token formatter: >= 1000 uses compact notation with
/// exactly one fraction digit, lowercased ("33.8k", "34.0k", "1.5m").
fn fmt_token_count(n: f64) -> String {
    let n = n.max(0.0);
    if n < 1000.0 {
        return format!("{}", n.round() as i64);
    }
    // Unit boundaries sit where one-decimal rounding would print "1000.0".
    let (val, unit) = if n >= 999_950_000.0 {
        (n / 1e9, "b")
    } else if n >= 999_950.0 {
        (n / 1e6, "m")
    } else {
        (n / 1e3, "k")
    };
    format!("{val:.1}{unit}")
}

/// Mirror the host's duration formatter: "54s", "3m 5s", "1h 2m 5s", "1d 2h 3m".
/// Sub-minute floors the seconds; above that seconds are rounded with carry.
fn fmt_elapsed_ms(ms: f64) -> String {
    let ms = ms.max(0.0);
    if ms < 60_000.0 {
        return format!("{}s", (ms / 1000.0).floor() as i64);
    }
    let mut d = (ms / 86_400_000.0).floor() as i64;
    let mut h = (ms % 86_400_000.0 / 3_600_000.0).floor() as i64;
    let mut m = (ms % 3_600_000.0 / 60_000.0).floor() as i64;
    let mut s = (ms % 60_000.0 / 1000.0).round() as i64;
    if s == 60 {
        s = 0;
        m += 1;
    }
    if m == 60 {
        m = 0;
        h += 1;
    }
    if h == 24 {
        h = 0;
        d += 1;
    }
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m {s}s")
    } else {
        format!("{m}m {s}s")
    }
}

/// Short display name from a resolved model id: "claude-opus-4-8" → "Opus 4.8",
/// "claude-3-5-haiku-20241022" → "Haiku 3.5". Handles Bedrock/Vertex-style ids
/// by substring search; unknown ids pass through (truncated).
fn model_display_name(id: &str) -> String {
    let lower = id.to_ascii_lowercase();
    let base = lower.split('[').next().unwrap_or(""); // strip "[1m]"-style suffix
    let tokens: Vec<&str> = base
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect();
    const FAMILIES: [&str; 5] = ["fable", "mythos", "opus", "sonnet", "haiku"];
    let Some(fi) = tokens.iter().position(|t| FAMILIES.contains(t)) else {
        return truncate_display(id, RAW_MODEL_MAX);
    };
    // Version segments are short digit runs; an 8-digit run is a date suffix.
    let is_ver = |t: &str| t.len() <= 2 && t.bytes().all(|b| b.is_ascii_digit());
    let mut ver: Vec<&str> = Vec::new();
    for t in &tokens[fi + 1..] {
        if !is_ver(t) {
            break;
        }
        ver.push(t);
    }
    if ver.is_empty() {
        // Older ids put the version before the family ("claude-3-5-haiku").
        for t in tokens[..fi].iter().rev() {
            if !is_ver(t) {
                break;
            }
            ver.insert(0, t);
        }
    }
    let fam = tokens[fi];
    let mut name = fam[..1].to_ascii_uppercase();
    name.push_str(&fam[1..]);
    if !ver.is_empty() {
        name.push(' ');
        name.push_str(&ver.join("."));
    }
    name
}

fn task_str<'a>(task: &'a Value, key: &str) -> &'a str {
    task.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

/// Single-line-safe copy of host-provided text.
fn sanitize(s: &str) -> String {
    s.replace(['\n', '\r', '\t'], " ")
}

/// True when tokenSamples (~5s of history) show the count still climbing,
/// i.e. the agent is receiving tokens right now rather than waiting on a tool.
fn is_receiving(task: &Value) -> bool {
    let Some(samples) = task.get("tokenSamples").and_then(|v| v.as_array()) else {
        return false;
    };
    let nums: Vec<f64> = samples.iter().filter_map(|v| v.as_f64()).collect();
    nums.len() >= 2 && nums[nums.len() - 1] > nums[0]
}

struct RowInput {
    id: String,
    identity: String,
    summary: String,
    right: String,
}

/// Decide eligibility and gather per-row pieces. Returns None to leave the row
/// on Claude Code's default rendering.
fn subagent_row_input(task: &Value, now_ms: f64) -> Option<RowInput> {
    let task_type = task_str(task, "type");
    // Only running tasks: completed rows freeze their elapsed/token text
    // host-side, which we can't reproduce (no endTime in the input).
    if task_str(task, "status") != "running" {
        return None;
    }
    let model = task_str(task, "model");
    // Without a model we'd add nothing over the default row.
    if model.is_empty() {
        return None;
    }
    let label = task_str(task, "label");
    match task_type {
        "local_agent" => {}
        "in_process_teammate" => {
            // These states carry host-side info we can't reproduce (queued
            // count, approval prompt), and the label text doubles as the
            // state flag — fall back to the default row for them.
            if label == "idle" || label == "awaiting approval" {
                return None;
            }
        }
        _ => return None,
    }

    let name = task_str(task, "name");
    let description = task_str(task, "description");
    let identity = if !name.is_empty() { name } else { description };
    if identity.is_empty() {
        return None;
    }
    let summary = if !label.is_empty() { label } else { description };
    let summary = if summary == identity { "" } else { summary };

    let mut right: Vec<String> = vec![model_display_name(model)];
    if task_type == "local_agent" {
        // Teammate rows deliberately omit elapsed: the default shows a
        // per-turn timer (turnStartTime), and startTime-based lifetime would
        // read as a wrong value in the same position.
        if let Some(start) = task.get("startTime").and_then(|v| v.as_f64()) {
            right.push(fmt_elapsed_ms(now_ms - start));
        }
    }
    let tokens = task.get("tokenCount").and_then(|v| v.as_f64()).unwrap_or(0.0);
    if tokens > 0.0 {
        let arrow = if is_receiving(task) { "↓ " } else { "" };
        right.push(format!("{arrow}{} tokens", fmt_token_count(tokens)));
    }

    Some(RowInput {
        id: task_str(task, "id").to_string(),
        identity: sanitize(identity),
        summary: sanitize(summary),
        right: right.join(" · "),
    })
}

fn build_subagent_rows(tasks: &[Value], columns: usize, now_ms: f64) -> Vec<(String, String)> {
    let rows: Vec<RowInput> = tasks
        .iter()
        .filter_map(|t| subagent_row_input(t, now_ms))
        .filter(|r| !r.id.is_empty())
        .collect();
    if rows.is_empty() {
        return Vec::new();
    }

    let name_col = rows
        .iter()
        .map(|r| display_width(&r.identity))
        .max()
        .unwrap_or(0)
        .clamp(NAME_COL_MIN, NAME_COL_MAX);

    rows.iter()
        .map(|r| {
            let identity = truncate_display(&r.identity, name_col);
            let right_w = display_width(&r.right);
            let fixed = name_col + NAME_SUMMARY_GAP + right_w + RIGHT_MIN_GAP;
            let content = if columns > fixed {
                // identity column · flexible summary · right-aligned metrics
                let summary_budget = columns - fixed;
                let summary = truncate_display(&r.summary, summary_budget);
                let pad = columns
                    - name_col
                    - NAME_SUMMARY_GAP
                    - display_width(&summary)
                    - right_w;
                format!(
                    "{identity}{}{summary}{}{}",
                    " ".repeat(name_col - display_width(&identity) + NAME_SUMMARY_GAP),
                    " ".repeat(pad),
                    r.right
                )
            } else {
                // Too narrow to right-align: identity + metrics, host truncates.
                format!("{} {}", identity, r.right)
            };
            (r.id.clone(), content)
        })
        .collect()
}

fn subagent_main() {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).expect("failed to read stdin");
    let data: Value = serde_json::from_str(&input).expect("invalid JSON on stdin");
    let columns = data.get("columns").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let empty = Vec::new();
    let tasks = data.get("tasks").and_then(|v| v.as_array()).unwrap_or(&empty);
    // NOTE: this mode must never touch the rate-limit cache — it runs every
    // ~300ms while tasks are active and would churn the file pointlessly.
    let now_ms = now_unix() * 1000.0;
    for (id, content) in build_subagent_rows(tasks, columns, now_ms) {
        println!("{}", serde_json::json!({ "id": id, "content": content }));
    }
}

fn now_unix() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64()
}

fn main() {
    if std::env::args().any(|a| a == "--version") {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if std::env::args().any(|a| a == "--subagent") {
        subagent_main();
        return;
    }

    let mut input = String::new();
    io::stdin().read_to_string(&mut input).expect("failed to read stdin");
    let data: Value = serde_json::from_str(&input).expect("invalid JSON on stdin");

    // ── Async git: start the lookup ASAP so it overlaps with line-1 work ────
    let ws_dir = data
        .pointer("/workspace/current_dir")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let current_dir = if !ws_dir.is_empty() {
        ws_dir.to_string()
    } else {
        data.get("cwd").and_then(|v| v.as_str()).unwrap_or("").to_string()
    };
    let git_rx = if !current_dir.is_empty() {
        Some(spawn_git_lookup(&current_dir))
    } else {
        None
    };

    // ── Line 1: model │ ctx │ 5h │ 7d ─────────────────────────────────────────
    let mut model = data
        .pointer("/model/display_name")
        .and_then(|v| v.as_str())
        .unwrap_or("Claude")
        .to_string();
    model = strip_context_suffix(&model);

    if let Some(effort) = data.pointer("/effort/level").and_then(|v| v.as_str()) {
        model = format!("{model} {DIM}{effort}{R}");
    }

    let mut parts = vec![model];

    if let Some(ctx) = data
        .pointer("/context_window/used_percentage")
        .and_then(|v| v.as_f64())
    {
        parts.push(fmt_metric("ctx", ctx, block_bar(ctx), None));
    }

    let now = now_unix();
    let empty_obj = Value::Object(serde_json::Map::new());
    let rate_limits_input = data.get("rate_limits").unwrap_or(&empty_obj);
    let cache_path = cache_path();
    let cached = read_cache(cache_path.as_deref(), now);

    let mut merged = serde_json::Map::new();
    for key in ["five_hour", "seven_day"] {
        let a = rate_limits_input.get(key).unwrap_or(&empty_obj);
        let b = cached.get(key).unwrap_or(&empty_obj);
        merged.insert(key.to_string(), pick_fresher(a, b, now));
    }
    let rate_limits = Value::Object(merged);
    write_cache(cache_path.as_deref(), &rate_limits, now);

    if let Some(five) = rate_limits
        .get("five_hour")
        .and_then(|v| fmt_rate_limit(v, Window::FiveHour, now))
    {
        parts.push(five);
    }
    if let Some(week) = rate_limits
        .get("seven_day")
        .and_then(|v| fmt_rate_limit(v, Window::SevenDay, now))
    {
        parts.push(week);
    }

    let line1 = parts.join(&format!(" {DIM}│{R} "));

    // ── Line 2: directory + git info ──────────────────────────────────────────
    let mut line2 = String::new();
    if let Some(rx) = git_rx {
        let git = collect_git_info(rx);
        if !git.toplevel.is_empty() {
            let toplevel_path = Path::new(&git.toplevel);
            let current_path = Path::new(&current_dir);
            let repo_name = toplevel_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| git.toplevel.clone());
            let rel_path = relpath(current_path, toplevel_path);

            let path_part = if rel_path == "." {
                repo_name
            } else {
                format!("{repo_name}/{rel_path}")
            };

            let git_info = if !git.branch.is_empty() && !git.commit_hash.is_empty() {
                format!("[{}] ({})", git.branch, git.commit_hash)
            } else if !git.commit_hash.is_empty() {
                format!("({})", git.commit_hash)
            } else if !git.branch.is_empty() {
                format!("[{}]", git.branch)
            } else {
                String::new()
            };

            line2 = format!("{CYAN}{path_part}{R}");
            if !git_info.is_empty() {
                line2.push_str(&format!(" {DIM}{git_info}{R}"));
            }
        } else {
            line2 = format!("{CYAN}{current_dir}{R}");
        }
    }

    // ── Output ────────────────────────────────────────────────────────────────
    println!("{line1}");
    if !line2.is_empty() {
        println!("{line2}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_count_matches_host_formatter() {
        assert_eq!(fmt_token_count(0.0), "0");
        assert_eq!(fmt_token_count(842.0), "842");
        assert_eq!(fmt_token_count(33_800.0), "33.8k");
        assert_eq!(fmt_token_count(34_000.0), "34.0k");
        assert_eq!(fmt_token_count(999_949.0), "999.9k");
        assert_eq!(fmt_token_count(999_950.0), "1.0m");
        assert_eq!(fmt_token_count(1_500_000.0), "1.5m");
    }

    #[test]
    fn elapsed_matches_host_formatter() {
        assert_eq!(fmt_elapsed_ms(0.0), "0s");
        assert_eq!(fmt_elapsed_ms(54_000.0), "54s");
        assert_eq!(fmt_elapsed_ms(59_999.0), "59s");
        assert_eq!(fmt_elapsed_ms(60_000.0), "1m 0s");
        assert_eq!(fmt_elapsed_ms(185_000.0), "3m 5s");
        assert_eq!(fmt_elapsed_ms(119_501.0), "2m 0s"); // seconds round + carry
        assert_eq!(fmt_elapsed_ms(3_725_000.0), "1h 2m 5s");
        assert_eq!(fmt_elapsed_ms(90_060_000.0), "1d 1h 1m");
    }

    #[test]
    fn model_names() {
        assert_eq!(model_display_name("claude-fable-5"), "Fable 5");
        assert_eq!(model_display_name("claude-opus-4-8"), "Opus 4.8");
        assert_eq!(model_display_name("claude-sonnet-5"), "Sonnet 5");
        assert_eq!(model_display_name("claude-haiku-4-5-20251001"), "Haiku 4.5");
        assert_eq!(model_display_name("claude-3-5-haiku-20241022"), "Haiku 3.5");
        assert_eq!(model_display_name("claude-fable-5[1m]"), "Fable 5");
        assert_eq!(
            model_display_name("us.anthropic.claude-sonnet-5-20250929-v1:0"),
            "Sonnet 5"
        );
        assert_eq!(model_display_name("sonnet"), "Sonnet");
        assert_eq!(model_display_name("some-custom-model"), "some-custom-model");
    }

    fn agent(id: &str, name: &str, label: &str, tokens: f64, samples: &[f64]) -> Value {
        serde_json::json!({
            "id": id,
            "name": name,
            "type": "local_agent",
            "status": "running",
            "description": "test task",
            "label": label,
            "startTime": 0.0,
            "model": "claude-haiku-4-5-20251001",
            "tokenCount": tokens,
            "tokenSamples": samples,
        })
    }

    #[test]
    fn rows_are_right_aligned_to_columns() {
        let tasks = vec![
            agent("t1", "a", "reading files", 12_400.0, &[1.0, 2.0]),
            agent("t2", "longer-name", "writing code", 45_200.0, &[5.0, 5.0]),
        ];
        let rows = build_subagent_rows(&tasks, 100, 60_000.0);
        assert_eq!(rows.len(), 2);
        for (_, content) in &rows {
            assert_eq!(display_width(content), 100);
        }
        assert!(rows[0].1.ends_with("Haiku 4.5 · 1m 0s · ↓ 12.4k tokens"));
        // flat samples → no arrow
        assert!(rows[1].1.ends_with("Haiku 4.5 · 1m 0s · 45.2k tokens"));
        // shared identity column: both summaries start at the same offset
        let off = |s: &str, needle: &str| {
            let b = s.find(needle).unwrap();
            display_width(&s[..b])
        };
        assert_eq!(off(&rows[0].1, "reading"), off(&rows[1].1, "writing"));
    }

    #[test]
    fn ineligible_tasks_fall_back_to_default() {
        let mut bash = agent("t1", "build", "cargo build", 0.0, &[]);
        bash["type"] = "local_bash".into();
        let mut done = agent("t2", "done", "finished", 10.0, &[]);
        done["status"] = "completed".into();
        let mut modelless = agent("t3", "nomodel", "working", 10.0, &[]);
        modelless["model"] = "".into();
        let mut idle = agent("t4", "mate", "idle", 10.0, &[]);
        idle["type"] = "in_process_teammate".into();
        let mut approval = agent("t5", "mate2", "awaiting approval", 10.0, &[]);
        approval["type"] = "in_process_teammate".into();
        let tasks = vec![bash, done, modelless, idle, approval];
        assert!(build_subagent_rows(&tasks, 100, 60_000.0).is_empty());
    }

    #[test]
    fn teammate_rows_omit_elapsed() {
        let mut mate = agent("t1", "reviewer", "reviewing main.rs", 45_200.0, &[1.0, 2.0]);
        mate["type"] = "in_process_teammate".into();
        let rows = build_subagent_rows(&[mate], 100, 60_000.0);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].1.ends_with("Haiku 4.5 · ↓ 45.2k tokens"));
        assert!(!rows[0].1.contains("1m 0s"));
    }

    #[test]
    fn narrow_columns_keep_identity_and_metrics() {
        let tasks = vec![agent("t1", "agent-name", "some long summary", 12_400.0, &[])];
        let rows = build_subagent_rows(&tasks, 20, 60_000.0);
        assert_eq!(rows[0].1, "agent-name Haiku 4.5 · 1m 0s · 12.4k tokens");
    }

    #[test]
    fn summary_truncates_before_metrics() {
        let tasks = vec![agent("t1", "abcd", &"x".repeat(80), 12_400.0, &[])];
        let rows = build_subagent_rows(&tasks, 60, 60_000.0);
        let content = &rows[0].1;
        assert_eq!(display_width(content), 60);
        assert!(content.contains('…'));
        assert!(content.ends_with("Haiku 4.5 · 1m 0s · 12.4k tokens"));
    }
}
