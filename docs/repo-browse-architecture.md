# Repository Browser — アーキテクチャ

`or --browse` / `b` / Cockpit → Repo Browse で開く画面の構造。
機能を足すときはここを読む。

## 1. モジュール構成

| ファイル | 役割 |
|----------|------|
| `src/app/browse.rs` | 状態定義、ファイル読み込み、非同期タスクの起動と回収 |
| `src/symbols.rs` | シンボルエンジン（画面から独立、[symbol-index.md](symbol-index.md) 参照） |
| `src/ui/browse.rs` | 描画 |
| `src/app/input_browse.rs` | キー処理（2 ペイン + 2 オーバーレイ） |

4 ファイルとも新規追加なので、行数はそのままこのブランチの追加行数になる。**行数はこの
表に書かない**。以前は書いてあったが、`src/app/browse.rs` と `src/symbols.rs` を触った
ラウンドのたびに更新が漏れ、直前の版では 2,870 行 / 1,868 行と書いてあったものが実際には
3,018 行 / 1,942 行だった。数えるのは 1 コマンドで済む。

```bash
wc -l src/app/browse.rs src/symbols.rs src/ui/browse.rs src/app/input_browse.rs
```

既存ファイルへの変更は最小限に抑えてある:

- `src/app/types.rs` — `AppState` に 2 バリアントと `is_repo_browse()`、`CockpitMenuItem` に 1 バリアント
- `src/app/mod.rs` — `browse` / `input_browse` のモジュール宣言、`browse_state: Option<BrowseState>` フィールド、`poll_browse_updates()` を polling ループへ
- `src/app/input.rs` — dispatch 2 アーム、file list に `b` の分岐
- `src/ui/mod.rs` — モジュール宣言と dispatch 1 行
- `src/config/keybindings.rs` — `repo_browse` / `symbol_outline` / `symbol_search` / `toggle_blame`
- `src/main.rs` — `--browse` フラグ
- `src/ui/help.rs` — ヘルプ項目
- `src/app/cockpit.rs` / `src/ui/cockpit.rs` — ブラウザを開く Cockpit メニュー項目
- `src/language.rs` — 追加 grammar の言語検出
- `src/lib.rs` — `pub mod symbols;`
- `src/syntax/parser_pool.rs` — 追加 grammar の parser pool 対応
- `src/queries/{bash,c_sharp,haskell,markdown,moonbit,zig}/tags.scm` — 各言語でシンボルを抽出する query
- `Cargo.toml` — `symbol_index` bench ターゲットの追加、package から `docs/` を exclude
- `benches/ui_rendering.rs` — `browse_render` グループ（8 節）／`benches/symbol_index.rs` は新規
- `tests/cli.rs` — バイナリを起動する e2e（7 節）

新しい依存クレートは追加していない（`Cargo.toml` の diff は上の 2 点だけ）。

## 2. 状態機械

プロジェクト原則 4「個別の真偽値フラグではなく状態機械」に従い、**画面・モード・ロード
状態を表す真偽値フラグは 1 つも足していない**。`BrowseState` のフィールドに `bool` は
存在せず、「どの画面か」「読み込み中か」「インデックスができたか」はすべて下の列挙型が
答える。

このブランチが追加した `bool` **フィールド**は 3 つで、どれも状態機械の代わりではない。
数えるときは関数引数（`render_tree` / `render_content` の `focused`、`outline_row` の
`selected`、`content_lines` の `bg_color`）を除くこと — 以前の版はそこを混同して 2 つと
書いていた。

```bash
git diff main -- src/ | grep -E '^\+[^+].*: *bool' | grep -v 'fn '
```

- `OpenFile::viewable`（`src/app/browse.rs`）— 読み終えた 1 ファイルの性質であって
  遷移ではない。描画側は `notice` の有無で分岐しており、この値を見るのは
  `start_browse_highlight()` のガードだけである（§3 参照）。列挙型にしても
  `Viewable | Unviewable` の 2 値にしかならず、遷移も持たない。
- `ChunkOutcome::stopped_early`（`src/symbols.rs`）— インデックス構築ワーカー 1 個の
  戻り値の一部。`App` にも `BrowseState` にも保持されず、`build_cancellable` が
  `IndexBuild::Cancelled` を返すかどうかの判定に使って捨てる。
- `Args::browse`（`src/main.rs`）— `--browse` の clap フラグ。CLI の引数表現であって
  実行中の状態ではなく、起動時に一度だけ読まれて `AppState` の初期値に落ちる。

```
AppState
 ├ RepoBrowseTree   ツリーペインにフォーカス
 └ RepoBrowseFile   ファイル内容ペインにフォーカス
```

`BrowseState` 内部:

```
paths:     LoadState<Vec<String>>
           NotLoaded → Loading → Loaded(paths) | Error(msg)

index:     IndexState
           Idle → Building → Ready(Arc<SymbolIndex>) | Failed

open_load: OpenLoad
           Idle → Pending { path, line, scroll, cancel } → Idle | Failed { path, message }

blame:     BlameState
           Off → Waiting { path } → Loading { path, cancel }
               → Ready { path, gutter } | Failed

overlay:   BrowseOverlay
           None | Outline { selected } | SymbolSearch { query, selected }

filter:    Option<ListFilter>   ← 既存のリストフィルタを再利用
```

`IndexState` が独立した列挙型なのが要点で、**インデックスは加速装置であって前提条件では
ない**。`Building` の間もツリー閲覧・ファイル閲覧・フィルタはすべて動く。インデックスを
参照する `o` / `s` / `gd` の 3 つだけが、オーバーレイを開かずにフッタへ理由を出す。

出るメッセージは 1 種類ではない。`o` と `gd` は**先に `open_is_pending()` を見る**ので、
ファイル読み込み中はインデックスの状態にかかわらず `Still opening this file` が勝つ
（`browse_run_go_to_definition()` と `open_browse_outline()`。どちらも
`index.ready()` の検査より**上**に置くこと自体が契約で、順序を入れ替えても
両方のメッセージは出てしまう）。読み込み中の `open` は行も symbols も持たない
placeholder なので、それを見て「シンボルがない」「定義が見つからない」と答えるのは
未読のファイルについての断定になる。`s` はリポジトリ全体の検索でありいま開いている
ファイルに依存しないため、この検査を持たない。

| キー | 読み込み中（index 未完了でも） | index が `Ready` 以外 | それ以外 |
|------|--------------------------------|------------------------|----------|
| `o` | `Still opening this file` | `Symbol index is still building` | 対象ファイルにシンボルが無ければ `No symbols in this file`、あればアウトラインを開く |
| `s` | （検査しない） | `Symbol index is still building` | 検索オーバーレイを開く |
| `gd` | `Still opening this file` | `Symbol index is still building` | 解決できなければ `No definition found` |

`Symbol index is still building` は `index.ready().is_none()` で出す。`IndexState::Failed`
もこの条件に入るため、構築に失敗した状態でもフッタは「構築中」と言い続ける。ヘッダは
`symbols: unavailable` を赤で出しているので画面内で食い違う（§8 の既知の制約）。

`OpenLoad` はファイルを開く途中の唯一の権威で、「読み込み中か」「どの path の結果を
待っているか」「どこへカーソルを置くか」を 1 箇所に持つ。

`BlameState` も同じ原則で、表示の有無・取得中・準備完了・失敗を 1 つの列挙型に持つ。
`Waiting` は blame 表示中に別ファイルを開き、そのファイルが viewable か判明するまでの
状態である。失敗理由は `IndexState::Failed` と同様にバリアントへ持たせず、フッタ用の
`BrowseState::status` へ入れる。

## 3. データフロー

```
open_repo_browse()
   │
   ├─ spawn_blocking: git ls-files -z --cached --others --exclude-standard ─┐
   │                （追跡ファイル + ignore されていない未追跡ファイル）    │ paths_receiver
   ▼                                                                       ▼
AppState::RepoBrowseTree                                        poll_browse_updates()
                                                                           │
                                                              set_paths() → rebuild_tree()
                                                                           │
                                                              start_symbol_index_build()
                                                                           │
                                                     spawn_blocking: SymbolIndex::build_cancellable
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
   ├─ 先行リクエストの cancel token を cancel、file/highlight receiver を破棄
   ├─ OpenLoad::Pending { path, line, scroll, cancel }
   │     └ open は読み込み中の placeholder
   ├─ 新しい file_receiver を設置
   │
   └─ spawn_blocking: load_file()
         ├ 各ステップを stage(cancel, ..) 越しに実行（cancel 済みなら closure を呼ばない）
         ├ metadata だけで判定: ディレクトリ / 8 MiB 超
         ├ 読み込み後に判定: 非 UTF-8 / 100,000 行超 / 1 行 10,000 bytes 超
         ├ build_file_patch()          ← 全行 context 行の擬似 patch
         └ build_plain_diff_cache()    ← tree-sitter を通さない
                    │
              deliver_file_load()      ← Ready は cancel 済みなら送らない
                    │ file_receiver
                    ▼
            poll_browse_updates()
               ├ path 不一致 → receiver を残して待機を継続
               ├ path 一致 + Err → install_file_load_failure()
               │                    └ OpenLoad::Failed
               ├ Pending 中の Disconnected
               │    └ "<path>: file loading task ended" → OpenLoad::Failed
               ├ path 一致 + Ok
               │    ├ install_open_file() → OpenLoad::Idle
               │    └ file_ready = true
               └ 4 receiver arm の処理後、file_ready == true
                    └ start_browse_highlight()
                         └ spawn_blocking: build_diff_cache()
                               ← tree-sitter ハイライト
                                  │ highlight_receiver
                                  ▼
                           apply_highlighted_cache()
                               ← パスが一致するときだけ差し替え
```

blame gutter を有効にするとき:

```
toggle_browse_blame() (`gb`)
   │
   ├─ 先行リクエストの cancel token を cancel、blame_receiver を破棄
   ├─ BlameState::Waiting { path }
   │
   └─ viewable な OpenFile だけ BlameState::Loading { path, cancel }
          └ spawn_blocking: crate::github::blame_file()
                    │
              deliver_blame_load()  ← cancel 済みなら送らない
                    │ blame_receiver
                    ▼
            poll_browse_updates()
               ├ path 不一致 → receiver を残して待機を継続
               ├ Err → BlameState::Failed + status
               └ Ok  → 行ごとの表示文字列を 1 回だけ準備
                        → BlameState::Ready { path, gutter }
```

blame 表示中に別ファイルを開くと、古い取得を cancel して `Waiting` へ移る。ファイル読み込み
完了後に `OpenFile::viewable` を検査してから取得を始めるため、バイナリ・上限超過ファイルへ
`git blame` を起動しない。取得結果と現在の open path が違う場合も poll 側で捨てる。

`build_plain_diff_cache()` は `expand_tabs` → `classify_line` → 文字列 interning の 1 パス
だけで、`ParserPool` を受け取らない。ハイライトを待たずにファイルを表示できるのはこのため
で、`build_diff_cache()` による差し替えは後追いで届く。

`std::fs::metadata` 1 回で決まるものが 2 つある。通常ファイルでなければ
（ディレクトリを含む）`file_metadata()` が `not a regular file` を返して
`FileLoad::Failed` になり、`MAX_VIEWABLE_FILE_BYTES = 8 * 1024 * 1024` 超は
`metadata.len()` の比較だけで notice になる。どちらも中身を読まない。残りの
3 つ — 非 UTF-8 のバイナリ notice、`MAX_VIEWABLE_FILE_LINES = 100_000`
（この値ちょうどは開ける）、最長行に対する `MAX_VIEWABLE_LINE_BYTES = 10_000` — は
ファイルを読んでからでないと判定できない。
一覧側は `git ls-files` の出力を全部パースしてから `MAX_BROWSE_FILES = 200_000` 件へ
truncate する。打ち切った事実は `total` に残り、フッタで件数を知らせる。

`BrowseState::open_is_pending()` は、開こうとしているファイルをまだ読み込んでいるかを
判定する唯一の入口である。読み込み中の `open` は placeholder なので、開いたファイルに
ついて答える処理は、未読ファイルを内容がないものとして扱わないよう、先にこのメソッドを
確認する。

`start_browse_highlight()` は `open.viewable` が false のときその場で return するので、
バイナリや上限超過のファイルがハイライターに渡ることはない。これは読んで確かめる約束では
なくテストで固定してある: `test_unviewable_files_never_start_background_highlighting` は
実際にバイナリファイルと 8 MiB 超のファイルを作って `browse_open_path` から settle させ、
`highlight_receiver` が 1 つも設置されないことを見る。ガードを外すと
`receivers were installed for ["binary", "oversized"]` と言って落ちる（2 種類とも報告するので、
片方だけ通るような直し方も検出できる）。

**擬似 patch のトリック**が設計上いちばん効いている。ファイル内容を
`@@ -1,N +1,N @@` + 全行先頭スペースの patch に変換して既存の `build_diff_cache` に
通すことで、

- ハイライト、テーマ、tab 展開、Vue/Svelte/Markdown の injection がそのまま効く
- レンダリング経路が diff view と 1 本のまま（改善が両方に効き、片方だけ腐らない）

同じ手が既に `build_pr_description_patch()` で使われていたので、それに倣った形。

`paths_receiver` / `index_receiver` / `file_receiver` / `highlight_receiver` /
`blame_receiver` はすべて `poll_browse_updates()` で `try_recv()` する。描画ループを
ブロックしない。

## 4. キー処理の階層

`handle_repo_browse_{tree,file}_input` の先頭で、上の層から順に食わせる。

```
1. オーバーレイ（Outline / SymbolSearch）  ← モーダル。開いていれば全部ここで消費
2. フィルタ入力バー（ツリーのみ）           ← 開いていれば文字入力を全部消費
3. 両ペイン共通（s / ? / Z / Ctrl-o）
4. シーケンス（ツリー: Space / と gg ／ ファイル: gb, gd, gf, gg）
5. 単一キー
```

**シーケンス層が必要な理由**: `filter` の既定値は `Space /` という 2 キーシーケンス
なので `matches_single_key` では絶対にマッチしない。既存の file list / diff view と
同じ `push_pending_key` / `try_match_sequence` の流儀に揃えてある。

**シンボル検索オーバーレイの入力規則**: 文字入力が優先で、`j` / `k` はクエリに入る。
選択移動は `↑` `↓` と `Ctrl-p` `Ctrl-n` のみ（クエリ全消しは `Ctrl-u`）。検索 UI で
`j` が使えないのは苛立つので意図的にこうしている。

## 5. キーバインド登録の注意

`KeybindingsConfig::validate()` は単一キーの重複を検出するが、loader は stderr へ
`Warning:` を出して起動を続ける。
新しい既定キーは既存と衝突しやすい:

- `b` … `rally_background` と衝突
- `o` … `filter_open` と衝突
- `s` … `suggestion` / `git_ops_stage_all` と衝突

3 つとも `is_context_compatible()` の `SCREEN_SPECIFIC_KEYS` に登録して回避している
（「その画面でしか生きないキー」の扱い）。キーを足すときは以下を全部触る:

1. `KeybindingsConfig` のフィールド
2. `Default` 実装
3. `validate()` の `bindings` 配列
4. `SCREEN_SPECIFIC_KEYS`（単一キーが本当に特定画面だけで有効な場合）
5. `Serialize` 実装の `serialize_entry`

blame gutter は file pane の既存シーケンス層へ `toggle_blame = ["g", "b"]` として登録する。
`gg` / `gd` / `gf` と同じ prefix を共有するが、2 キー目が重ならない。
`validate()` はシーケンスの先頭キーだけを `sequence_prefixes` に入れるため、
`toggle_blame` を `SCREEN_SPECIFIC_KEYS` に登録してはいけない。登録すると `g` prefix
全体との単一キー衝突が context compatible として抑制される。

## 6. 描画

`src/ui/browse.rs`。zen mode ではヘッダとフッタを落として全面を 2 ペインにする。

- ツリーペイン: `LoadState` に応じて「読み込み中スピナー」「エラー」「ツリー」
- 内容ペイン: 未選択 / バイナリ・巨大ファイルの notice / 内容
- 行番号の gutter は最小幅が `LINE_NUMBER_WIDTH = 5` 列で、`gutter_width()` が総行数の
  桁数に合わせて広げる。100,000 行のファイルでは 6 桁になり、上限を 999,999 行より先へ
  引き上げても自動で再び広がる。カーソル行は gutter を黄色にし、`diff.bg_color` が
  有効なら行背景も付ける
- `BlameState::Ready` では blame gutter を行番号の左へ置く。取得結果の到着時に commit
  ごとの full（sha + author + relative time）／time なし／identity の文字列を表示幅で
  切り詰めて準備し、同一 commit の連続行は空欄にする。描画時は準備済み `&str` を参照する
  だけで、狭くなる順に full → time なし → identity → 非表示へ落とす。未コミット行は
  zero SHA と epoch 0 を出さず `Uncommitted` と表示する
- 擬似 patch 由来の `LineType::Header` 行（`@@ ... @@`）は描画前に除外する。
  `content_window()` はヘッダが前置プレフィックスであることを使って連続スライスを
  借りるだけなので、走査量はビューポート幅で決まる。**ファイルの N 行目はキャッシュの
  N+1 行目**という対応関係になっている
- 各行の先頭スパンからは context マーカーのスペース 1 個を剥がす

オーバーレイは `clear_overlay_area()` を通してから中央に描く。この関数は対象領域を
`Clear` したうえで、左隣の列に CJK などの倍幅文字があれば、そのセルをスペースへ置き換える。
倍幅文字が左境界をまたぐと ratatui の buffer diff が次セルを飛ばし、境界線が端末へ送られない
ためである。右端側は、先頭セルを上書きした時点で continuation cell にスペースが残るため、
同じ補修を必要としない。`render_outline`（60%×70%）と `render_symbol_search`（80%×70%）は
ともにこの関数を使う。`src/ui/browse.rs` に新しいオーバーレイを加える場合も、裸の `Clear`
ではなく必ず `clear_overlay_area()` を通すこと。continuation cell はテキスト上では
スペースなので、テキストスナップショットだけではこの誤りを検出できない。補修を外すと
`test_overlay_left_border_survives_a_wide_glyph_straddling_it` が落ちる。

`overlay_rect()` の中央配置と境界内への収まりは
`test_overlay_rect_is_centred_and_bounded` が検証する。100×40 では 80%×50% の矩形が
(10, 10) の 80×20 になり、10×4 でも端末内に収まる。
`test_symbol_search_overlay_is_reviewably_clipped_in_a_tiny_terminal` は 20×5 で実際に
シンボル検索オーバーレイを描画し、クリップ結果をスナップショットで固定する。

## 7. テスト

**このドキュメントはテスト本数を書かない。** 以前は書いてあったが 3 ラウンド連続で
実際と違う値が載り続けた（直前の版は合計 194 本、`src/app/browse.rs` 79 本、
`src/symbols.rs` 52 本としていたが、同じ版のコードを下のコマンドで数えると
`app::browse` 82 本、`symbols::tests` 53 本だった）。本数が要るときは数える。読む側が
知りたいのはたいてい「どこに何のテストがあるか」なので、そちらを表に残す。

| 場所 | 内容 |
|------|------|
| `src/app/browse.rs` | `git ls-files` パース、擬似 patch 変換、ツリー、フィルタ、カーソル/スクロール、ファイル読み込みとそのキャンセル |
| `src/symbols.rs` | 言語別抽出のスナップショット、境界（空/未対応/構文エラー/CJK）、インデックス、スコアリング、構築キャンセル |
| `src/ui/browse.rs` | 描画のインラインスナップショット、極小端末のクリップ、倍幅文字の境界補修 |
| `src/app/input_browse.rs` | **シナリオテスト**（ツリー移動→開く→スクロール→戻る、フィルタ→取消、アウトライン→ジャンプ→戻る 等） |
| `src/main.rs` | リポジトリが無くても起動を許すフラグ集合（`--browse` を含む） |
| `tests/cli.rs` | `assert_cmd` でバイナリを起動する e2e（非 git ディレクトリ、GitHub remote の無い git repo） |

上 4 ファイルはこのブランチで新規追加なので、モジュールのテスト数がそのまま新規本数に
なる。数えるコマンド:

```bash
cargo test --lib -- --list | grep ': test$' | awk -F'::' '{print $1"::"$2}' | sort | uniq -c
```

このコマンドが数えるのは `#[test]` として登録されたものだけである。`fn test_symbol(..)` の
ようなヘルパ関数は `grep -c 'fn test_'` には引っかかるがこの一覧には出ないので、
grep で数えると `src/symbols.rs` と `src/app/input_browse.rs` は 1 本ずつ多く出る。

`src/main.rs` と `tests/cli.rs` は既存ファイルなので `--lib` には出ず、この一覧にも
現れない。それぞれ `cargo test --bin or -- --list` / `cargo test --test cli -- --list`
で数える。どちらもモジュール単位ではなくターゲット単位の合計しか出ないので、このブランチの
追加分だけを知りたいときは `git diff main -- <file> | grep '^+' | grep -E 'fn +[a-z_]+\('`
を使う（`tests/cli.rs` のテスト関数名は `test_` で始まらない）。

### insta インラインスナップショットの更新について

この環境には `cargo-insta 1.46.3`（`~/.cargo/bin/cargo-insta`）が入っている。インライン
スナップショットもこれで更新できる。

```bash
cargo insta test --accept --lib -- <test_name>   # 実行して差分をその場で受理
cargo insta test --lib -- <test_name>            # 受理せず .pending-snap だけ書く
cargo insta accept                               # 溜まった .pending-snap を適用
cargo insta review                               # 対話的に 1 件ずつ確認して受理
```

`--accept` と `cargo insta accept` はソース中の `assert_snapshot!(..., @"...")` を直接
書き換える。このとき escape の要らない内容は raw 文字列 `@r"..."` から `@"..."` へ
正規化されることがあるので、受理したあとは該当テストの再実行と
`cargo fmt --all -- --check` まで通すこと。実際 `src/ui/browse.rs` のインライン
スナップショットを 1 つ壊して `--accept` させると、内容は元に戻るが `@r"` だけが `@"` に
変わった差分が残る。

`INSTA_FORCE_UPDATE=1` を素の `cargo test` に付けてもインラインスナップショットは
書き換わらない。`.pending-snap` は出るがソースはそのままで、テストは失敗したままになる。
cargo-insta が使えない環境でのフォールバックは

```bash
cargo test --lib <test_name> 2>&1 | sed -n '/Snapshot Summary/,/insta review/p'
```

で `+new results` 側を読み、ソース中のインライン文字列を手で差し替える方法だが、
既定の手順ではない。

### tokio ランタイムが要るテスト

`browse_open_path()` は `spawn_blocking` を呼ぶので、素の `#[test]` では
"there is no reactor running" で panic する。`#[tokio::test] async fn` にすること。

## 8. 既知の制約

| 制約 | 影響 | 対処案 |
|------|------|--------|
| インデックスは `BrowseState` 新規作成時の 1 回だけ | 同一 root への再入では再構築せず、外部変更後も古いまま。構築中は二重起動しない。閉じて開き直すと増分更新ではなく全件をゼロから再構築 | 現在は refresh key がない。`R` で再構築、あるいは既存の file watcher に相乗り |
| ファイル一覧も `BrowseState` 新規作成時の 1 回だけ | 同一 root への再入では再列挙せず、新規ファイルがツリーに出ない。閉じて開き直すと全件を再列挙 | 同上 |
| 検索結果は 200 件で打ち切り | `MAX_SYMBOL_SEARCH_RESULTS` 件だけを返して下位を失う。返却順は全一致を sort した場合と同じで、ranking は失わない | `matches` は打ち切り後の `hits.len()` なので `200 matches` で飽和し、打ち切りを判別できない。ページングは未実装 |
| 横スクロールなし | 長い行が切れる | ratatui の `Paragraph` に横スクロールを足す |
| 行内の折り返しなし | 同上 | 折り返すとカーソル行の計算が視覚行ベースになる（`pr_description` と同じ問題） |
| `gd` は最初に解決した識別子へ飛ぶ | 列は見ず、重複と一般的な keyword を除いた識別子を行頭から走査する。`foo.bar()` は `foo` に定義があればそこへ飛び、なければ `bar` を試す | 候補が複数あるとき既存の `SymbolPopupState` を出す |
| references（`gr`）未実装 | query にある `@reference.*` も、`SymbolKind::from_capture` が `definition.` だけを扱うため抽出時に捨てる。C/C++/Swift と repo 同梱の 6 query には capture 自体がない | Rust/TS/JS/Go/Python/Ruby/Java/Lua/PHP は既存 capture を使って同じ仕組みを拡張できる。C/C++/Swift と同梱の `c_sharp`/`zig`/`bash`/`haskell`/`moonbit`/`markdown` は query 拡張が必要で、それまでは一部言語のみの対応になる |
| ファイル内容のキャッシュは 1 枚だけ | `OpenLoad` を経由するため UI thread は塞がないが、再訪のたびにファイル全体を読み直す | `DiffCacheStore` のように LRU を持たせる |
| インデックス構築が失敗しても `o` / `s` / `gd` は「構築中」と言う | 3 つとも判定が `index.ready().is_none()` なので `IndexState::Failed` も同じ枝に落ちる。ヘッダは `symbols: unavailable` を赤で出しているので、同じ画面の 2 箇所が食い違う | フッタ側でも `IndexState` を match し、`Failed` のときは `BrowseState::status` に入っている実際の失敗理由を出す |

### リバートで確認したゲート

以下はすべて「該当箇所を戻すと名前を挙げたテストが落ちる」ことを実際にやって確認して
ある。書き換えるときはこの対応表を維持すること。

- **追い越されたバックグラウンドファイル読み込みのキャンセル**
  - `browse_open_path_at` の `cancel.cancel()` を消す →
    `test_opening_a_second_file_cancels_the_first_request`（`first_request.is_cancelled()`）、
    `test_opening_a_newer_file_supersedes_an_in_flight_background_load`
    （`receiver replacement alone must not be mistaken for work cancellation`）
  - `stage()` を無条件 `Some(work())` に戻す →
    `test_cancelled_stage_does_not_run_its_work`、
    `test_pre_cancelled_file_load_skips_metadata_work`、
    `test_pre_cancelled_file_contents_skip_read_work`
  - `deliver_file_load()` の `if !cancel.is_cancelled()` 送信ガードを外す →
    `test_cancelled_ready_load_is_not_delivered`

- **インデックス構築のキャンセル**
  - `SymbolIndex::build_cancellable` のメタデータ prefilter から
    `PREFILTER_CANCEL_POLL_INTERVAL` の poll を消す →
    `test_cancelled_build_stops_the_metadata_prefilter`
  - `index_chunk` の先頭の poll を消す → `test_cancelled_build_stops_scanning_early`
  - `open_repo_browse` の `state.cancel_token.cancel()`（別 root への再入）を消す →
    `test_open_repo_browse_replaces_different_root_and_cancels_old_session`
  - `start_symbol_index_build` の `IndexBuild::Cancelled { .. } => {}` を「何か送る」に
    変える → `test_pre_cancelled_real_symbol_index_build_delivers_nothing`。これは実物の
    `SymbolIndex::build_cancellable` を `spawn_blocking` 越しに走らせ、cancel 済み
    セッションの build がチャネルへ何も流さない（receiver が `None` を返す）ことまで見る

- **ハイライターに渡さない**: §3 の
  `test_unviewable_files_never_start_background_highlighting`

- **オーバーレイ左境界の補修**: `clear_overlay_area()` の倍幅セル置換ループを消す →
  `test_overlay_left_border_survives_a_wide_glyph_straddling_it`

### 自動ゲートで守られていない性質

- **再入が「走っている最中の」build を止めること**: 上のゲートは (a) cancel された build が
  早期に止まる、(b) 再入が古いセッショントークンを cancel する、(c) cancel 済みトークンで
  起動した実 build が何も届けない、をそれぞれ固定する。残っている穴は 1 つで、
  「すでに走行中の build を再入が途中で止める」経路を通したテストがない。(c) は cancel が
  build 開始**前**に済んでいる場合であって、`open_repo_browse` による再入がトリガではない。
  壊れても描画結果は変わらず、無駄な CPU 消費と古いインデックスによる上書きだけが発生する。

- **行数・行長キャップが「構築の前」に評価されること**: `load_file_contents` は両キャップを
  lines ベクタ・擬似 patch・plain キャッシュの構築より**前**に評価する。この順序こそがキャップの
  目的そのもの（巨大ファイルに対して行あたりの描画状態を確保しないこと）だが、順序を入れ替えても
  返る notice は一字一句同じなので、**テストは一切気付かない**（revert 掃討で実測）。
  50 万行や minify されたバンドルなら、拒否される前に全行ベクタと patch とキャッシュを確保し、
  キャップが防ぐはずだった数秒のフリーズとメモリスパイクをそのまま起こす。
  キャップの**判定**自体は
  `test_line_count_cap_admits_its_own_value_and_rejects_one_more` と
  `test_line_length_cap_admits_its_own_value_and_rejects_one_more` が包含境界で固定しており、
  バイト単位で測っていることは `test_the_line_length_cap_counts_bytes_not_characters` が
  固定している。守られていないのは**位置**だけである。並べ替えるときは自分で確かめること。

- **描画コストが O(viewport) であること**: `cargo test` が固定するのは形だけである。
  `test_content_window_finds_the_content_start_by_prefix_not_by_filtering` は「ヘッダは
  前置プレフィックスであり、window は連続スライス」という契約を固定するが、コストは
  測らない。コストの実測は `benches/ui_rendering.rs` の `browse_render` グループに
  ある。これは `.github/workflows/benchmark.yml` が 26 行目の
  `cargo bench --bench ui_rendering --bench diff_parsing` で実行する 2 本の bench の
  1 本で、36 行目の `alert-threshold: '150%'` の対象である。`render_content` の
  per-frame path に `open.cache.lines.iter().filter(..)` の walk をそのまま注入した実測は
  次のとおり（Criterion の点推定、同一マシン・同一セッション、`--bench ui_rendering --
  browse_render`）。

  | 状態 | browse_render/200 | browse_render/30000 |
  |---|---|---|
  | 現状（clean, 1 回目） | 45.692 us | 49.350 us |
  | 現状（clean, 2 回目） | 47.840 us | 46.182 us |
  | O(file) の walk を注入（`.collect()`） | 46.903 us | 89.295 us |
  | 同じ walk を `.count()` に | 46.023 us | 66.182 us |

  `.collect()` の 30000 は clean のどちらと比べても `89.295 / 49.350 = 1.809`、
  `89.295 / 46.182 = 1.934` で、150% 閾値を確実に超える。200 行ケースは
  `46.903 / 45.692 = 1.027` と誤差の範囲で、O(viewport) の性質どおりファイル長では動かない。

  - **assertion は検出しない**: walk を注入しても `cargo test` は 1485 / 82 / 18 と
    3 つのテストバイナリとも全部通り、`cargo bench --bench ui_rendering -- --test` も
    `browse_render/200` `browse_render/30000` の両方が Success になる。
    検出するのは実際の Criterion sampling run だけである。これは assertion の
    skip ではない。`assert_browse_frames_are_comparable` は実行されており、
    `assert_eq!(small_lines.len(), 18)` を 17 に変えると `-- --test` は
    `benches/ui_rendering.rs:98` で `left: 18 / right: 17` と panic する。

  - **感度に下限がある**: 同じ walk を `.collect()` ではなく `.count()` にすると
    `66.182 / 49.350 = 1.341`、clean の速いほうと比べても `66.182 / 46.182 = 1.433` で、
    どちらも 1.5 の閾値を下回るため alert しない。しかも上の 2 本の clean baseline 自身が
    browse_render/200 で 1.047 倍、browse_render/30000 で 1.069 倍ばらついており、
    どちらの baseline に当たるかで比が 1.34〜1.43 と動く。alert が拾うのは甚大な回帰で
    あり、微細な回帰ではない。

  - **alert は何も block しない**: `benchmark.yml` は 38 行目で
    `fail-on-alert: false` を設定し、trigger は 5-8 行目の `workflow_dispatch` と
    週次 cron だけである。発火しても PR に紐づかない non-blocking run に comment
    するだけである。

  `lint.yml` は 46 行目の
  `cargo clippy --all-targets --workspace -- -D warnings` を実行するため bench を
  compile するが、実行はしない。trigger は 3-6 行目の `pull_request` と
  `push: branches: [main]` なので、feature ブランチでは PR を開くまで走らない。`cargo bench --bench ui_rendering -- --test` を
  job に追加しても gate できるのは frame-shape assertion であり、O(viewport) の
  cost ではない。この cost property に対する安価な assertion 形式は現在ない。

## 9. 拡張ポイント

- **新しいオーバーレイ**: `BrowseOverlay` にバリアントを足し、
  `handle_browse_overlay_input` と `render_overlay` の match を埋める。
  コンパイラが漏れを教える
- **新しいペイン内アクション**: `handle_repo_browse_file_input` に
  `matches_single_key` の分岐を足す。`self` の借用と `browse_state` の可変借用が
  衝突するので、**判定を先に bool へ落としてから state を取る**のが定石
  （既存コードがその形になっている）
- **行アノテーションの追加**: blame と同様、取得 lifecycle は `BrowseState` の列挙型、
  描画データは結果到着時に作るサイドカーとして持たせ、`render_content` の per-frame
  経路では借用だけにする。
