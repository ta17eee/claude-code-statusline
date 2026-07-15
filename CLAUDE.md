# CLAUDE.md

## Overview

Claude Code 用のカスタムステータスライン。実装は Rust バイナリ (`rust/`)。v2.0.0 で Python 実装 (`statusline.py`) から全面移行し、リポジトリからは削除済み。旧実装の設計判断は以下のセクションに移行の経緯として残している (過去形の記述はすべて「もう存在しない旧実装について」を指す)。

## Git 情報の取得設計 (gix/gitoxide)

Line 2 の git 情報取得は `gix::discover()` → `repo.head()` → `repo.head_id()` を同一プロセス内で呼ぶ (`rust/src/main.rs`)。サブプロセスは起動しない。

### 旧 Python 実装 (git CLI サブプロセス3回) からの移行経緯

Python版では以下の理由で git CLI サブプロセス3回の個別呼び出しに落ち着いていた:

1. `git rev-parse --show-toplevel --abbrev-ref HEAD --short HEAD` (1回) → `--abbrev-ref` が後続の `HEAD` にも影響し `--short HEAD` が文字列 "HEAD" を返す
2. `--show-toplevel --short HEAD` + `branch --show-current` (2回) → コミットなしリポジトリで `--short HEAD` が失敗し `--show-toplevel` も道連れになる
3. 3回個別実行 → 全ケースで正しく動作するが、subprocess spawn のコストがかかる

`gix` はこれらを構造的に解決するため、この3分割は不要になった:

- `repo.head()` が `Symbolic`/`Unborn`/`Detached` の enum を返し、CLI のようにフラグの副作用で文字列を汚染する余地がない
- `workdir()` / `head()` / `head_id()` が完全に独立した呼び出しで、一つの失敗が他を道連れにしない
- 短縮ハッシュの曖昧性解消 (`Id::shorten_or_id()`) は `core.abbrev` を読み、pack+loose オブジェクト全体をスキャンして一意になるまで長さを伸ばすループを持つ。git CLI と同じ設計思想で、実機の大規模リポジトリ (pack内66,154+loose 2,294オブジェクト) で両者が9文字で完全一致することを確認済み

### タイムアウト安全装置 (意図的な設計)

`discover_git_info()` は別スレッド (`spawn_git_lookup`) で実行し、メインスレッドは `recv_timeout(500ms)` で待つ (`rust/src/main.rs`)。gix の呼び出しは通常1ms未満で終わるが、NFS 等ネットワークマウントされたリポジトリでの I/O stall に備えた安全装置で、Python版の subprocess timeout+kill の設計思想を引き継いでいる。

**この安全装置を「同期呼び出しで十分速いから」という理由で削除しないこと。** スレッド生成のオーバーヘッドは同一プロセス内のOSスレッド生成 (数十〜数百マイクロ秒) であり、subprocess spawn (~50ms) とは別次元のコストのため、安全装置を維持してもRust版の実行コスト (~5ms) への影響は誤差程度に収まる。

### gix の最小フィーチャーセット

`gix = { default-features = false, features = ["sha1"] }` で構成している (`rust/Cargo.toml`)。`discover`/`head`/`head_id`/`workdir`/`git_dir` に関わるモジュールは feature gate なしで無条件公開されており、`sha1` (ObjectId の根幹) のみが必須。フル機能版とのリリースビルドバイナリサイズ差は誤差程度 (1.8MB vs 1.9MB、LTO/dead-code-elimination のため) だが、クレート数とクリーンビルド時間は大きく減る (`cargo tree` 行数で 298 vs 530、クリーンビルドで約11秒 vs 約20秒)。バイナリサイズ削減目的で最小フィーチャーにしていると誤解しないこと — 目的はコンパイル時間と依存クレート数(サプライチェーン面)の削減。

## レートリミットの色設計

- 色は使用率ではなくペース予測値に基づく
- 予測80%以下: 緑 (color_pct = 0)
- 予測80%〜95%: 緑〜黄 (color_pct 0→40)
- 予測95%超: 黄〜赤 (color_pct 40→100)。赤の閾値は経過に応じて予測150%→100%に圧縮
- 経過10%未満: ペース予測が不安定なため生の使用率にフォールバック
- 5h: リセット時刻を常に表示
- 7d: 予測90%以上でリセット時刻を表示 (24時間以上先なら日付、24時間以内なら時刻)

## クロスセッションキャッシュ

- `~/.claude/statusline-cache.json` にレートリミット情報を原子的に書き込み。Rust版は `std::fs::rename` を使用し、Windows では `MoveFileExW` + `MOVEFILE_REPLACE_EXISTING` が使われる (Python版の `os.replace` の Windows 実装と同一設計であることをソースレベルで確認済み。`std::fs::rename` を `os.rename` 相当と誤解して独自のアトミック置換処理を実装しないこと)
- 各セッション起動時にキャッシュを読み、同一ウィンドウ (`resets_at` 一致) では `used_percentage` が高い方を採用
- ウィンドウリセット後 (`resets_at < now`) はキャッシュを 0% にフォールバック
- 7日超の古いキャッシュは無視 — ただし `_write_cache`/`write_cache` は毎回 `ts` を現在時刻で上書きするため、この安全装置はステータスラインが動き続ける限り実質発動しない。汚染からの回復は下記の「ライブ優先」ロジックに依存している

### 汚染耐性: `resets_at` が一致しない場合はライブ優先 (2026-07-15 修正)

`_pick_fresher`/`pick_fresher` で、ライブ側・キャッシュ側の双方が期限切れではないのに `resets_at` が異なる場合、**ライブデータを無条件に採用する** (以前は「大きい方を採用」だったが、これがバグの温床だった)。

**理由**: 1アカウントは同時に1つのウィンドウにしか属さないため、正当な運用下では「両方とも未期限切れなのに `resets_at` が異なる」状態はほぼ起こり得ない (正当な「他セッションの新しい値を借りる」ケースは、期限切れ側の `resets_at` が `_expire()` で `None` になり `ar is None`/`br is None` 分岐で処理されるため、この分岐を経由しない)。つまりこの分岐は実質的に汚染時にしか到達しない。

「大きい方を採用」だと、いずれかのセッションが誤って (バグ等で) 未来すぎる `resets_at` を報告した瞬間、その値がキャッシュに書き込まれ、以後**全セッション**がそれを引き継いでしまう。しかも上記の通り `ts` が毎回更新されるため7日ルールも効かず、汚染された `resets_at` を実際の壁時計が追い越すまで直りようがなかった。

ライブ優先にすることで、正しいライブデータを持つセッションは次回実行時にキャッシュを正しい値で即座に上書きする (自己修復)。**この分岐を「大きい方 (新しいウィンドウ) を優先」に戻さないこと。**

## Rust版でのWindows対応について

Claude Code はステータスラインスクリプトの stdout を常にパイプでキャプチャし、コンソールに直結しない。そのため Rust の標準出力 (`println!`) は Windows でも UTF-8 バイト列をそのまま書き込み、Python版が行っていた `sys.stdout.reconfigure(encoding='utf-8')` のような対応は不要 (該当コード自体が存在しない)。この点をコードレビュー等で「Windows対応が漏れている」と誤指摘しないこと。
