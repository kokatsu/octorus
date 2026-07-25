# セッションログ — Pillar A / B 実装時の判断と発見

次のセッションが「なぜこうなっているのか」を再調査しなくて済むように、
決定と行き止まりを残す。実装の詳細は
[symbol-index.md](symbol-index.md) / [repo-browse-architecture.md](repo-browse-architecture.md)
にある。ここは**判断の記録**。

## 1. 調査で確認したこと

### 競合の位置づけ

| カテゴリ | 具体例 | 欠けているもの |
|----------|--------|---------------|
| エディタ | Zed, Helix, VS Code, Neovim | LSP 前提の導入コスト。編集機能は Viewer には過剰。PR 文脈が後付け |
| git TUI | tig, lazygit, gitui | 構文ハイライト付きのコード閲覧、シンボル移動、PR 文脈 |
| 表示系 CLI | bat, delta | ナビゲーション、リポジトリ横断 |
| 検索系 | ripgrep, ast-grep | 「読む」ための常設 UI |
| エージェント CLI | Claude Code, Codex, opencode, Qwen Code, CodeWhale | 人間が読むための画面（生成・編集が主眼） |
| SaaS レビュー | CodeRabbit, Greptile, Devin review, Sourcegraph | **手元のワーキングツリーが見えない** |

「ターミナル上でリポジトリ全体をコードとして読む統合 UI」は事実上の空白地帯だった、
というのが結論。

参照した記事:
- [tree-sitter Code Navigation](https://tree-sitter.github.io/tree-sitter/4-code-navigation.html)
- [tree-sitter tags CLI](https://tree-sitter.github.io/tree-sitter/cli/tags.html)
- [Best Claude Code Alternatives for Terminal Coding in 2026](https://kilo.ai/articles/claude-code-alternatives-for-terminal)
- [The State of AI Code Review in 2026](https://dev.to/rahulxsingh/the-state-of-ai-code-review-in-2026-trends-tools-and-whats-next-2gfh)

### 決定的だった技術的発見

**octorus が既に依存している tree-sitter クレートの 12 個が `TAGS_QUERY` を export
していた。** GitHub のコードナビゲーションが使っているのと同じ `tags.scm` である。

これで方針が確定した:「LSP の土俵で戦わず、**導入コストゼロのコード知能**で差別化する」。
エディタは汎用性ゆえに LSP を捨てられないので、これは構造的に真似しにくい。

`tree-sitter-tags` クレートは**不要**だった（`tree_sitter::Query::new` が tags.scm の
ディレクティブをそのまま受理する）。依存を 1 つも増やさずに済んでいる。

## 2. 実装方針の判断

### やったこと

| 判断 | 理由 |
|------|------|
| ファイル内容を「全行 context の擬似 patch」に変換して `build_diff_cache` に通す | レンダリング経路を 1 本に保つ。ハイライト・テーマ・injection の改善が diff view と Browse の両方に効く。既に `build_pr_description_patch` が同じ手を使っていた |
| `git ls-files` に `--others --exclude-standard` を付ける | エージェントが今さっき書いた未コミットファイルが見えないビューワは、いちばん必要なときに盲目になる。**これは仕様であって最適化ではない** |
| 読み取り専用（編集モードを作らない） | Viewer 特化が差別化の本体。編集したくなったら `gf` で `$EDITOR` に渡す |
| `IndexState` を独立した列挙型にする | インデックスは加速装置であって前提条件ではない。`Building` 中も全機能が使えることを型で表現する |
| プレーンキャッシュを同期・即時、ハイライトを背景タスク | 20,000 行のファイルを開いてもキーストロークが詰まらない。既存の `ensure_diff_cache` と同じ構え |
| シンボル検索で `j`/`k` をクエリ入力にする | 検索 UI で `j` が打てないのは苛立つ。移動は `↑↓` と `Ctrl-p/n` |
| `src/symbol.rs`（grep 版）を残す | diff view の `gd` は「インデックス無しで即座に動く」ことに価値がある。置き換えは慎重に |

### やらなかったこと（意図的）

| 見送り | 理由 |
|--------|------|
| rayon の導入 | `std::thread::scope` で足りる。依存を増やさない |
| references（`gr`） | 定義に比べて件数が桁違い。メモリと構築時間を実測してから |
| 横スクロール / 行折り返し | カーソル行の計算が視覚行ベースになり、`pr_description` と同じ複雑さを抱える。別 PR で |
| ファイル内容の LRU キャッシュ | まず 1 枚で出して、遅さを感じてから入れる |
| インデックスの自動再構築 | file watcher への相乗りが自然だが、Browse とローカル diff モードの排他関係を整理してから |

## 3. 踏んだ罠（同じ穴を掘らないために）

### tree-sitter / tags

1. **TypeScript の TAGS_QUERY は JS を継承する前提** — 連結しないと
   `class Widget {}` からシンボルが 1 つも取れない。`HIGHLIGHTS_QUERY` と同じ構造。
   一方 **C++ の tags.scm は自己完結**しており、C との連結は不要（highlights とは違う）
2. **Rust の tags.scm は impl 内の fn を 2 パターンでマッチさせる** — 同じ
   `function_item` ノードに `@definition.method` と `@definition.function` が付く。
   素朴に処理するとアウトラインに 2 行出て、包含スタックが自分自身を包含と判断して
   depth が割れる。`collapse_duplicate_tags()`（name ノードのバイトオフセットをキーに、
   kind 具体性 → 定義ノード幅で 1 件に絞る）で解決
3. **`impl Foo` は `@reference.implementation`** — 定義ではないのでアウトラインに
   出ない。上流の設計判断
4. **Markdown の `atx_heading` は兄弟** — ネストを持つのは `section`。
   `(section (atx_heading ...))` で捕まえる
5. **tree-sitter の column はバイト単位** — octorus は全部文字単位なので
   `char_column()` で変換が要る。CJK 識別子で実際にズレる
6. **C# は `queries/tags.scm` を持っているのに定数を export していない** — 自前で
   同梱するしかない（highlights も同じ状況で、既に前例があった）

### octorus のコードベース

7. **`filter` の既定は `Space /` という 2 キーシーケンス** — `matches_single_key` では
   絶対にマッチしない。ツリーペインにシーケンス解決層が必要だった
8. **キーバインドの重複検証が起動時エラーになる** — `b`/`o`/`s` はそれぞれ
   `rally_background`/`filter_open`/`suggestion` と衝突する。
   `is_context_compatible()` の `SCREEN_SPECIFIC_KEYS` に登録して回避。
   キー追加時は 4 箇所（フィールド / Default / validate の配列 / Serialize）を全部触る
9. **`DiffCache` は `Debug` を実装していない**（`Rodeo` を持つため）— `OpenFile` には
   手書きの `Debug` を入れた。テストで `.unwrap()` するのに要る
10. **`self` の借用と `browse_state` の可変借用が衝突する** — `matches_single_key` は
    `&self` を取るので、判定を先に bool へ落としてから `browse_state.as_mut()` を取る
11. **`spawn_blocking` を呼ぶコードは `#[tokio::test]` でないと panic** —
    "there is no reactor running"
12. **Cockpit のメニュー項目を増やすと 4 つのスナップショットと 3 つの単体テストが落ちる** —
    `CockpitMenuItem::ALL` の要素数が型に出ているので、コンパイラは教えてくれない箇所がある

### ツールチェイン

13. **この環境に `cargo-insta` が入っていない** — インラインスナップショットは
    `INSTA_FORCE_UPDATE=1` でも更新されず、`.pending-snap` も出ない。
    `cargo test --lib <name> 2>&1 | sed -n '/Snapshot Summary/,/insta review/p'` で
    差分を読んで手で書き換えた。次セッションで `cargo install cargo-insta` できるなら
    その方が速い

## 4. 数値

計測環境: octorus 自身（162 ファイル、約 70k LOC）、release ビルド。

| 操作 | 実測 |
|------|------|
| `SymbolIndex::build`（121 ファイル / 3,439 シンボル） | 約 250 ms（バックグラウンド） |
| `search("browse")` → 44 hits | 約 0.40 ms |
| `search("sym")` → 180 hits | 約 0.37 ms |
| `definitions("BrowseState")` | 約 1 µs |

追加した行数:

| ファイル | 行 |
|----------|-----|
| `src/symbols.rs` | 1,101 |
| `src/app/browse.rs` | 1,063 |
| `src/app/input_browse.rs` | 1,044 |
| `src/ui/browse.rs` | 786 |
| `benches/symbol_index.rs` | 173 |
| `src/queries/*/tags.scm` | 77 |

テスト 94 本追加（symbols 33 / app::browse 30 / app::input_browse 19 / ui::browse 12）。
3 ゲートすべて通過:

```
cargo clippy --all-targets -- -D warnings   # 警告ゼロ
cargo test                                   # 1,482 件グリーン
cargo fmt --check                            # クリーン
```

## 5. 次にやること

[roadmap/code-archaeology.md](roadmap/code-archaeology.md) の Phase C-1（blame
オーバーレイ）から。C-1 だけでも単体で価値があり、C-3（commit → PR 解決）まで行くと
octorus 固有の体験になる。各フェーズは独立した PR にできる粒度で設計してある。

その前に片付けると気持ちがいい小物:

- `.github/workflows/benchmark.yml` に `--bench symbol_index` を足して回帰監視に載せる
- インデックスとファイル一覧の手動再構築（`R`）
- `gd` の候補が複数あるときの選択ポップアップ（既存の `SymbolPopupState` を流用）
