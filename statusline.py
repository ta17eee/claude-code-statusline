#!/usr/bin/env python3
"""Claude Code status line: two-line display with block/braille progress bars."""
__version__ = '1.0.0'

import json
import os
import re
import subprocess
import sys
import time

if '--version' in sys.argv:
    print(__version__)
    sys.exit(0)

if sys.platform == 'win32':
    for _stream in (sys.stdout, sys.stdin):
        try:
            if hasattr(_stream, 'reconfigure'):
                _stream.reconfigure(encoding='utf-8')
        except Exception:
            pass

data = json.load(sys.stdin)

# ── ANSI helpers ──────────────────────────────────────────────────────────────
R   = '\033[0m'
DIM = '\033[2m'


def gradient(pct: float) -> str:
    """Green (low) → red (high) 24-bit colour escape."""
    if pct < 50:
        r = int(pct * 5.1)
        return f'\033[38;2;{r};200;80m'
    else:
        g = int(200 - (pct - 50) * 4)
        return f'\033[38;2;255;{max(g, 0)};60m'


# ── Bar renderers ─────────────────────────────────────────────────────────────
BLOCKS  = ' ▁▂▃▄▅▆▇█'   # 9 levels (index 0 = empty, 8 = full)
BRAILLE = ' ⡀⣀⣄⣤⣦⣶⣷⣿'   # 9 levels (index 0 = empty, 8 = full)


def _bar(chars: str, pct: float, width: int = 8) -> str:
    levels = len(chars) - 1
    pct    = min(max(pct, 0), 100)
    level  = pct / 100
    bar    = ''
    for i in range(width):
        seg_start = i / width
        seg_end   = (i + 1) / width
        if level >= seg_end:
            bar += chars[levels]
        elif level <= seg_start:
            bar += chars[0]
        else:
            frac = (level - seg_start) / (seg_end - seg_start)
            bar += chars[min(int(frac * levels) + 1, levels)]
    return bar


def block_bar(pct: float, width: int = 8) -> str:
    return _bar(BLOCKS, pct, width)


def braille_bar(pct: float, width: int = 8) -> str:
    return _bar(BRAILLE, pct, width)


def fmt_metric(label, pct, bar_fn, color_pct=None):
    p = round(pct)
    cpct = pct if color_pct is None else color_pct
    return f'{DIM}{label}{R} {gradient(cpct)}{bar_fn(pct)}{R} {p}%'


# ── Rate limit pace projection ───────────────────────────────────────────────
WINDOW_SECONDS = {'five_hour': 5 * 3600, 'seven_day': 7 * 86400}
PACE_THRESHOLD = 0.10


def fmt_rate_limit(label, limit_data, window_key):
    used_pct = limit_data.get('used_percentage')
    if used_pct is None:
        return None

    resets_at = limit_data.get('resets_at')
    window = WINDOW_SECONDS.get(window_key)

    if resets_at is None or window is None:
        return fmt_metric(label, used_pct, braille_bar)

    now = time.time()
    elapsed = now - (resets_at - window)
    elapsed_ratio = elapsed / window if window > 0 else 0

    if elapsed_ratio < PACE_THRESHOLD or elapsed <= 0:
        return fmt_metric(label, used_pct, braille_bar)

    projected = used_pct * window / elapsed
    progress = (elapsed_ratio - PACE_THRESHOLD) / (1 - PACE_THRESHOLD)

    if projected <= 100:
        color_pct = projected / 2
    else:
        max_excess = 100 - progress * 90  # 100 (early) → 10 (late)
        excess = projected - 100
        color_pct = 50 + min(excess / max_excess, 1) * 50

    result = fmt_metric(label, used_pct, braille_bar, color_pct=color_pct)

    if projected >= 90:
        reset_local = time.localtime(resets_at)
        reset_str = time.strftime('%H:%M', reset_local)
        result += f' {DIM}@{reset_str}{R}'

    return result


# ── Line 1: model │ ctx │ 5h │ 7d ─────────────────────────────────────────────
model = data.get('model', {}).get('display_name', 'Claude')
model = re.sub(r'\s*\([^)]*context\)', '', model)

ctx_size = data.get('context_window', {}).get('context_window_size')
if ctx_size is not None:
    if ctx_size >= 1_000_000:
        size_label = f'{ctx_size // 1_000_000}M'
    elif ctx_size >= 1_000:
        size_label = f'{ctx_size // 1_000}K'
    else:
        size_label = str(ctx_size)
    model = f'{model} {size_label}'

parts = [model]

ctx = data.get('context_window', {}).get('used_percentage')
if ctx is not None:
    parts.append(fmt_metric('ctx', ctx, block_bar))

rate_limits = data.get('rate_limits') or {}

five = fmt_rate_limit('5h', rate_limits.get('five_hour', {}), 'five_hour')
if five is not None:
    parts.append(five)

week = fmt_rate_limit('7d', rate_limits.get('seven_day', {}), 'seven_day')
if week is not None:
    parts.append(week)

line1 = f' {DIM}│{R} '.join(parts)

# ── Line 2: directory + git info ──────────────────────────────────────────────
current_dir = data.get('workspace', {}).get('current_dir') or data.get('cwd', '')

CYAN = '\033[38;2;100;180;255m'


def _git(*args, cwd: str):
    """Run a git command; return stdout stripped, or '' on error."""
    try:
        return subprocess.check_output(
            ['git', '--no-optional-locks', '-C', cwd, *args],
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
    except Exception:
        return ''


line2 = ''
if current_dir:
    toplevel = _git('rev-parse', '--show-toplevel', cwd=current_dir)
    if toplevel:
        commit_hash = _git('rev-parse', '--short', 'HEAD', cwd=current_dir)
        branch      = _git('branch', '--show-current', cwd=current_dir)

        repo_name = os.path.basename(toplevel)
        rel_path  = os.path.relpath(current_dir, toplevel)

        path_part = repo_name if rel_path == '.' else repo_name + '/' + rel_path.replace(os.sep, '/')
        if branch and commit_hash:
            git_info = f'[{branch}] ({commit_hash})'
        elif commit_hash:
            git_info = f'({commit_hash})'
        elif branch:
            git_info = f'[{branch}]'
        else:
            git_info = ''

        line2 = f'{CYAN}{path_part}{R}'
        if git_info:
            line2 += f' {DIM}{git_info}{R}'
    else:
        line2 = f'{CYAN}{current_dir}{R}'

# ── Output ────────────────────────────────────────────────────────────────────
print(line1)
if line2:
    print(line2)
