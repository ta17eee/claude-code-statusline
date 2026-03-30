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
- 予測100%以下: `projected / 2` で緑〜黄 (時間に依存しない)
- 予測100%超: ウィンドウ経過に応じて黄〜赤の範囲を圧縮 (終盤ほど厳しく)
- 経過10%未満: ペース予測が不安定なため生の使用率にフォールバック
- 予測90%以上: リセット時刻 (@HH:MM) を表示
