//! Claude Code status line: two-line display with block/braille progress bars.
//! Rust port of statusline.py — see ../CLAUDE.md for design rationale.

use chrono::{Local, TimeZone};
use serde_json::Value;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

fn now_unix() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64()
}

fn main() {
    if std::env::args().any(|a| a == "--version") {
        println!("{}", env!("CARGO_PKG_VERSION"));
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
