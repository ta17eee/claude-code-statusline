# CLAUDE.md

## Overview

Claude Code 用のカスタムステータスライン。Python スクリプト1ファイルで構成。

## Git コマンドの設計判断

Line 2 の git 情報取得は3回の個別呼び出しで行っている。これは意図的な設計。

### 統合を試みた経緯と結論

1. `git rev-parse --show-toplevel --abbrev-ref HEAD --short HEAD` (1回)
   → `--abbrev-ref` が後続の `HEAD` にも影響し、`--short HEAD` がハッシュではなく文字列 "HEAD" を返す。順序を変えても解決しない

2. `git rev-parse --show-toplevel --short HEAD` + `git branch --show-current` (2回)
   → コミットが存在しないリポジトリで `--short HEAD` が失敗し、`--show-toplevel` の結果も道連れになる

3. 3回個別に実行 (現在の実装)
   → すべてのケース (detached HEAD、コミットなし、worktree) で正しく動作する

**3回の呼び出しを統合しようとしないこと。** いずれも `rev-parse` / `branch` で ~5ms 程度のため、パフォーマンス上の問題はない。

## レートリミットの色設計

- 色は使用率ではなくペース予測値に基づく
- 予測80%以下: 緑 (color_pct = 0)
- 予測80%〜95%: 緑〜黄 (color_pct 0→40)
- 予測95%超: 黄〜赤 (color_pct 40→100)。赤の閾値は経過に応じて予測150%→100%に圧縮
- 経過10%未満: ペース予測が不安定なため生の使用率にフォールバック
- 5h: リセット時刻を常に表示
- 7d: 予測90%以上でリセット時刻を表示 (24時間以上先なら日付、24時間以内なら時刻)

## クロスセッションキャッシュ

- `~/.claude/statusline-cache.json` にレートリミット情報を原子的に書き込み (`os.replace`)
- 各セッション起動時にキャッシュを読み、同一ウィンドウ (`resets_at` 一致) では `used_percentage` が高い方を採用
- ウィンドウリセット後 (`resets_at < now`) はキャッシュを 0% にフォールバック
- 7日超の古いキャッシュは無視
