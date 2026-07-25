# Repository Browser — アーキテクチャ

`or --browse` / `b` / Cockpit → Repo Browse で開く画面の構造。
機能を足すときはここを読む。

## 1. モジュール構成

| ファイル | 行数 | 役割 |
|----------|------|------|
| `src/app/browse.rs` | 1,063 | 状態定義、ファイル読み込み、非同期タスクの起動と回収 |
| `src/app/input_browse.rs` | 1,044 | キー処理（2 ペイン + 2 オーバーレイ） |
| `src/ui/browse.rs` | 786 | 描画 |
| `src/symbols.rs` | 1,101 | シンボルエンジン（画面から独立、[symbol-index.md](symbol-index.md) 参照） |

既存ファイルへの変更は最小限に抑えてある:

- `src/app/types.rs` — `AppState` に 2 バリアント、`CockpitMenuItem` に 1 バリアント
- `src/app/mod.rs` — `browse_state: Option<BrowseState>` フィールド、`poll_browse_updates()` を polling ループへ
- `src/app/input.rs` — dispatch 2 行、file list に `b` の分岐
- `src/ui/mod.rs` — dispatch 1 行
- `src/config/keybindings.rs` — `repo_browse` / `symbol_outline` / `symbol_search`
- `src/main.rs` — `--browse` フラグ
- `src/ui/help.rs` — ヘルプ項目

## 2. 状態機械

プロジェクト原則 4「個別の真偽値フラグではなく状態機械」に従い、真偽値フラグは 1 つも
足していない。

```
AppState
 ├ RepoBrowseTree   ツリーペインにフォーカス
 └ RepoBrowseFile   ファイル内容ペインにフォーカス
```

`BrowseState` 内部:

```
paths:   LoadState<Vec<String>>
         NotLoaded → Loading → Loaded(paths) | Error(msg)

index:   IndexState
         Idle → Building → Ready(Arc<SymbolIndex>) | Failed

overlay: BrowseOverlay
         None | Outline { selected } | SymbolSearch { query, selected }

filter:  Option<ListFilter>   ← 既存のリストフィルタを再利用
```

`IndexState` が独立した列挙型なのが要点で、**インデックスは加速装置であって前提条件では
ない**。`Building` の間もツリー閲覧・ファイル閲覧・フィルタはすべて動く。
`o` / `s` / `gd` だけが「まだ構築中」とフッタに出して何もしない。

## 3. データフロー

```
open_repo_browse()
   │
   ├─ spawn_blocking: git ls-files ──────────────┐
   │                                             │ paths_receiver
   ▼                                             ▼
AppState::RepoBrowseTree              poll_browse_updates()
                                                 │
                                    set_paths() → rebuild_tree()
                                                 │
                                    start_symbol_index_build()
                                                 │
                                    spawn_blocking: SymbolIndex::build
                                                 │ index_receiver
                                                 ▼
                                       IndexState::Ready(Arc<..>)
                                                 │
                                       refresh_open_file_symbols()
```

ファイルを開くとき:

```
browse_open_path(path, line)
   │
   ├─ load_file()  同期・即時
   │     ├ metadata チェック（ディレクトリ / 8 MiB 超）
   │     ├ UTF-8 でなければバイナリ扱い
   │     ├ build_file_patch()          ← 全行 context 行の擬似 patch
   │     └ build_plain_diff_cache()    ← ハイライト無し、~1 ms
   │
   └─ spawn_blocking: build_diff_cache()  ← tree-sitter ハイライト
             │ highlight_receiver
             ▼
        apply_highlighted_cache()  ← パスが一致するときだけ差し替え
```

**擬似 patch のトリック**が設計上いちばん効いている。ファイル内容を
`@@ -1,N +1,N @@` + 全行先頭スペースの patch に変換して既存の `build_diff_cache` に
通すことで、

- ハイライト、テーマ、tab 展開、Vue/Svelte/Markdown の injection がそのまま効く
- レンダリング経路が diff view と 1 本のまま（改善が両方に効き、片方だけ腐らない）

同じ手が既に `build_pr_description_patch()` で使われていたので、それに倣った形。

チャネルは 3 本とも `poll_browse_updates()` で `try_recv()` する。描画ループを
ブロックしない。

## 4. キー処理の階層

`handle_repo_browse_{tree,file}_input` の先頭で、上の層から順に食わせる。

```
1. オーバーレイ（Outline / SymbolSearch）  ← モーダル。開いていれば全部ここで消費
2. フィルタ入力バー（ツリーのみ）           ← 開いていれば文字入力を全部消費
3. 両ペイン共通（s / ? / Z / Ctrl-o）
4. シーケンス（ツリー: Space / と gg ／ ファイル: gd, gf, gg）
5. 単一キー
```

**シーケンス層が必要な理由**: `filter` の既定値は `Space /` という 2 キーシーケンス
なので `matches_single_key` では絶対にマッチしない。既存の file list / diff view と
同じ `push_pending_key` / `try_match_sequence` の流儀に揃えてある。

**シンボル検索オーバーレイの入力規則**: 文字入力が優先で、`j` / `k` はクエリに入る。
選択移動は `↑` `↓` と `Ctrl-p` `Ctrl-n` のみ。検索 UI で `j` が使えないのは苛立つので
意図的にこうしている。

## 5. キーバインド登録の注意

`KeybindingsConfig::validate()` は単一キーの重複を検出して**起動時にエラーにする**。
新しい既定キーは既存と衝突しやすい:

- `b` … `rally_background` と衝突
- `o` … `filter_open` と衝突
- `s` … `suggestion` / `git_ops_stage_all` と衝突

3 つとも `is_context_compatible()` の `SCREEN_SPECIFIC_KEYS` に登録して回避している
（「その画面でしか生きないキー」の扱い）。キーを足すときは以下 4 箇所を全部触る:

1. `KeybindingsConfig` のフィールド
2. `Default` 実装
3. `validate()` の `bindings` 配列
4. `Serialize` 実装の `serialize_entry`

## 6. 描画

`src/ui/browse.rs`。zen mode ではヘッダとフッタを落として全面を 2 ペインにする。

- ツリーペイン: `LoadState` に応じて「読み込み中スピナー」「エラー」「ツリー」
- 内容ペイン: 未選択 / バイナリ・巨大ファイルの notice / 内容
- 行番号は 5 桁の gutter。カーソル行は gutter を黄色にし、`diff.bg_color` が有効なら
  行背景も付ける
- 擬似 patch 由来の `LineType::Header` 行（`@@ ... @@`）は描画前に除外する。
  **ファイルの N 行目はキャッシュの N+1 行目**という対応関係になっている
- 各行の先頭スパンからは context マーカーのスペース 1 個を剥がす

オーバーレイは `Clear` を敷いてから中央に描く。`overlay_rect()` は端末が極小でも
有効な矩形を返す（`test_render_in_a_tiny_terminal_does_not_panic` で 20×5 を確認）。

## 7. テスト

新規テスト 94 本。

| 場所 | 本数 | 内容 |
|------|------|------|
| `src/symbols.rs` | 33 | 言語別抽出のスナップショット、境界（空/未対応/構文エラー/CJK）、インデックス、スコアリング |
| `src/app/browse.rs` | 30 | `git ls-files` パース、擬似 patch 変換、ツリー、フィルタ、カーソル/スクロール、ファイル読み込み |
| `src/app/input_browse.rs` | 19 | **シナリオテスト**（ツリー移動→開く→スクロール→戻る、フィルタ→取消、アウトライン→ジャンプ→戻る 等） |
| `src/ui/browse.rs` | 12 | 描画のインラインスナップショット |

### insta インラインスナップショットの更新について

この環境には `cargo-insta` が入っていない。`INSTA_FORCE_UPDATE=1` はインライン
スナップショットには効かない（`.pending-snap` も出ない）。更新手順:

```bash
cargo test --lib <test_name> 2>&1 | sed -n '/Snapshot Summary/,/insta review/p'
```

で `+new results` 側を読み、ソース中のインライン文字列を手で差し替える。
`cargo insta review` が使える環境ならそちらが速い。

### tokio ランタイムが要るテスト

`browse_open_path()` は `spawn_blocking` を呼ぶので、素の `#[test]` では
"there is no reactor running" で panic する。`#[tokio::test] async fn` にすること。

## 8. 既知の制約

| 制約 | 影響 | 対処案 |
|------|------|--------|
| インデックスはセッション開始時の 1 回だけ | 閲覧中に外部でファイルが変わってもシンボルは古いまま | `R` で再構築、あるいは既存の file watcher に相乗り |
| ファイル一覧も 1 回だけ | 新規ファイルがツリーに出ない | 同上 |
| 検索結果は 200 件で打ち切り | 広いクエリで下位が見えない | 件数表示はしている。ページングは未実装 |
| 横スクロールなし | 長い行が切れる | ratatui の `Paragraph` に横スクロールを足す |
| 行内の折り返しなし | 同上 | 折り返すとカーソル行の計算が視覚行ベースになる（`pr_description` と同じ問題） |
| `gd` は最初に解決した識別子へ飛ぶ | `foo.bar()` で `foo` が先に当たる | 候補が複数あるとき既存の `SymbolPopupState` を出す |
| references（`gr`）未実装 | 定義には飛べるが参照は辿れない | tags.scm には `@reference.*` も入っているので同じ仕組みで作れる |
| ファイル内容のキャッシュは 1 枚だけ | ファイルを行き来すると毎回読み直し | `DiffCacheStore` のように LRU を持たせる |

## 9. 拡張ポイント

- **新しいオーバーレイ**: `BrowseOverlay` にバリアントを足し、
  `handle_browse_overlay_input` と `render_overlay` の match を埋める。
  コンパイラが漏れを教える
- **新しいペイン内アクション**: `handle_repo_browse_file_input` に
  `matches_single_key` の分岐を足す。`self` の借用と `browse_state` の可変借用が
  衝突するので、**判定を先に bool へ落としてから state を取る**のが定石
  （既存コードがその形になっている）
- **blame などの行アノテーション**: `OpenFile` にサイドカーの `Vec<...>` を持たせ、
  `render_content` の gutter を拡張するのが素直。
  → [roadmap/code-archaeology.md](roadmap/code-archaeology.md)
