# Symbol Index — 技術リファレンス

`src/symbols.rs` に実装した tree-sitter tags ベースのシンボルエンジンの詳細。
言語追加やクエリ調整をするときはここを読む。

## 1. 何をしているか

tree-sitter の `tags.scm` クエリを CST に対して実行し、`@definition.*` キャプチャと
`@name` キャプチャのペアからシンボルを取り出している。GitHub のコードナビゲーション
（"Go to definition" / "Find all references"）が使っているのと同じクエリ資産である。

```
source ──parse──> Tree ──Query(tags.scm)──> QueryMatch*
                                              │
                            ┌─────────────────┴──────────────────┐
                            │ @name           → 名前とその位置    │
                            │ @definition.xxx → 種別と包含範囲    │
                            └─────────────────┬──────────────────┘
                                              ↓
                              collapse_duplicate_tags()   ← 同一 name ノードの重複排除
                                              ↓
                              ソート（start_byte 昇順, end_byte 降順）
                                              ↓
                              包含スタックで depth を算出
                                              ↓
                                        Vec<Symbol>
```

`Symbol` は `{ name, kind, line (1-based), column (0-based chars), depth }`。

## 2. 言語別 tags クエリの供給元

`SupportedLanguage::tags_query()` (`src/language.rs`) が返す。

| 言語 | 供給元 | 備考 |
|------|--------|------|
| Rust | `tree_sitter_rust::TAGS_QUERY` | 14 パターン |
| TypeScript / TSX | **JS + TS の連結** (`TYPESCRIPT_COMBINED_TAGS_QUERY`) | 後述の落とし穴 2 |
| JavaScript / JSX | `tree_sitter_javascript::TAGS_QUERY` | 11 パターン |
| Go | `tree_sitter_go::TAGS_QUERY` | |
| Python | `tree_sitter_python::TAGS_QUERY` | |
| Ruby | `tree_sitter_ruby::TAGS_QUERY` | |
| C | `tree_sitter_c::TAGS_QUERY` | |
| C++ | `tree_sitter_cpp::TAGS_QUERY` | **自己完結**。C との連結は不要（highlights とは違う） |
| Java | `tree_sitter_java::TAGS_QUERY` | |
| Lua | `tree_sitter_lua::TAGS_QUERY` | |
| PHP | `tree_sitter_php::TAGS_QUERY` | |
| Swift | `tree_sitter_swift::TAGS_QUERY` | |
| C# | `src/queries/c_sharp/tags.scm` | クレートに `queries/tags.scm` はあるが定数を export していない |
| Zig | `src/queries/zig/tags.scm` | 上流に tags.scm なし |
| Bash | `src/queries/bash/tags.scm` | 上流に tags.scm なし |
| Haskell | `src/queries/haskell/tags.scm` | 上流に tags.scm なし |
| MoonBit | `src/queries/moonbit/tags.scm` | vendored 文法 |
| Markdown | `src/queries/markdown/tags.scm` | 見出しをアウトラインにする |
| Svelte / Vue / CSS / MarkdownInline | `None` | 意図的。SFC の `<script>` は埋め込み言語側の担当、CSS に名前付きエンティティは無い |

`None` の集合はテスト `test_languages_without_tags_query_are_intentional` で固定して
いる。言語を追加したらこのテストも更新すること — 「うっかり未対応のまま」を防ぐための
ガードである。

## 3. 実装中に踏んだ落とし穴（重要）

### 3.1 `Query::new` は tags.scm のディレクティブを受け入れる

`tags.scm` には `#strip!` / `#select-adjacent!` / `#not-eq?` といった、`tree-sitter-tags`
クレート固有のディレクティブが含まれる。`tree_sitter::Query::new` はこれらを
general predicate として受理し、コンパイルエラーにならない。したがって
**`tree-sitter-tags` クレートへの依存は不要**で、素の `Query` + `QueryCursor` で足りる。

（`#not-eq?` などのテキスト述語が実行時に評価されるかどうかには依存していない。
評価されなくても余分なシンボルが 1 つ増えるだけで、壊れはしない。）

### 3.2 TypeScript の TAGS_QUERY は JavaScript を継承する前提

`tree_sitter_typescript::TAGS_QUERY` には TS 固有パターン（`function_signature`,
`interface_declaration`, `module` 等）しか入っていない。`class_declaration` や
`function_declaration` は JavaScript 側にある。

連結しないと `export class Widget {}` から**シンボルが 1 つも取れない**。
これは `HIGHLIGHTS_QUERY` の `TYPESCRIPT_COMBINED_QUERY` とまったく同じ構造なので、
`TYPESCRIPT_COMBINED_TAGS_QUERY` を同じ場所に並べてある。

C++ は逆で、`tags.scm` は自己完結している（`class_specifier` も `struct_specifier` も
自前で持っている）。`highlights` の癖から類推して連結すると重複が出るので注意。

### 3.3 Rust: 同じノードが 2 パターンにマッチする

`tree-sitter-rust` の tags.scm は impl ブロック内の関数を 2 回マッチさせる。

```scm
(declaration_list (function_item name: (identifier) @name) @definition.method)
(function_item     name: (identifier) @name)                @definition.function
```

`@definition.method` と `@definition.function` が**同じ `function_item` ノード**に付く。
素直に処理すると、

- アウトラインに `new` が 2 行出る
- 包含スタックが「自分自身を包含している」と判断して depth が 0 と 1 に割れる

対策が `collapse_duplicate_tags()`。**name ノードのバイトオフセットをキー**にして
1 件だけ残す。優先順位は `(kind_specificity, definition ノードの幅)` の辞書順で、
`Method` が `Function` より具体的、同種なら範囲が狭い方が勝つ。

### 3.4 Rust: `impl Foo` はアウトラインに出ない

`(impl_item type: (type_identifier) @name !trait) @reference.implementation` —
上流が定義ではなく**参照**として扱っている。したがって `impl` ブロックの見出しは
アウトラインに現れず、中のメソッドが直接並ぶ。これは上流の設計判断であり、
octorus 側では変えていない（変えたければ Rust だけ自前クエリに差し替える）。

### 3.5 Markdown: 見出しは兄弟ノードなのでネストが取れない

`atx_heading` は互いに包含関係を持たない。ネストを持つのは `section` の方。

```scm
; ✗ これだと全部 depth 0
(atx_heading heading_content: (inline) @name) @definition.heading

; ✓ section を捕まえると `##` が `#` の下に入る
(section (atx_heading heading_content: (inline) @name)) @definition.heading
```

### 3.6 Zig: `const Foo = struct {}` は variable_declaration

型らしきものが型宣言ノードではなく、初期化子にコンテナ宣言を持つ変数宣言として
表現される。

```scm
(variable_declaration (identifier) @name (struct_declaration)) @definition.class
```

### 3.7 Bash: トップレベル代入だけを拾う

`variable_assignment` を無条件に拾うと関数ローカル変数でアウトラインが埋まる。
`(program ...)` でアンカーして、トップレベルのみに限定している。

### 3.8 Haskell: 定義式ごとに 1 マッチ

複数の等式で定義された関数（`f [] = ...` / `f (x:xs) = ...`）は等式ごとにマッチする。
depth 算出後に**隣接する同一 `(name, kind, depth)` を 1 つに畳む**処理で吸収している。
「隣接のみ」なのが重要で、別の impl ブロックにある同名メソッドは別物として残る。

### 3.9 tree-sitter の column はバイト単位

`Node::start_position().column` はバイトオフセットを返す。octorus は表示幅計算も
カーソル位置もすべて文字単位で扱うので、`char_column()` で変換している。
CJK 識別子や、行頭に日本語コメントがあるケースで実際にズレる。

## 4. 検索スコアリング

`fuzzy_score_lowered(lowered_name, needle)`。**private** で、**両引数とも小文字化済みが
前提**である。`needle` 側は `LoweredNeedle` が構築時に保証し、名前側は `from_files()` が
`lowered_names` として事前計算したものを渡す（キーストロークごとにシンボル 1 個ずつ
小文字化を走らせないため）。公開 API から単体のスコアを取る手段はなく、`search()` の
返却順として観測する。

| 層 | スコア | 例（needle = `parse`） |
|----|--------|----------------------|
| 完全一致 | 10,000 − 長さ | `parse` |
| 前方一致 | 8,000 − 長さ | `parse_line` |
| 単語境界の後の部分一致 | 6,000 − 位置 − 長さ | `do_parse`（`_` `-` `.` `:` の直後） |
| 部分一致 | 4,000 − 位置 − 長さ | `reparsed` |
| 部分列 | 2,000 − min(ギャップ, 1,000) − 長さ | `please_advance_rest_of_set` |

「長さ」は `chars().count()`（バイト数ではない）。部分列層のギャップに 1,000 の上限が
あるのが層の分離を成立させている要で、これがないと散らばった部分列マッチが下の層まで
落ちうる。同点は「名前が短い順 → パス順 → 行番号順」で決着させる（描画順が実行ごとに
変わらないようにするため）。

`search(query, limit)` は全件をスコアリングしたあと、**全体ソートではなく
`select_nth_unstable_by` による top-N 部分選択**で `limit` 件まで切ってから並べ替える
（判断の根拠となる実測値は関数内のコメントに残してある）。さらに直近の
`(needle, limit)` 1 組の結果をメモ化しているので、同じクエリを同じ limit で呼び直しても
再スキャンは起きない。**limit が違えばメモは効かない** — 呼び口を複数持つと再計算に
なるだけでなく、返る集合そのものが食い違う。UI 側の唯一の呼び口は
`BrowseState::symbol_search_hits()` で、描画・選択クランプ・Enter 解決の 3 者がここを
通る。

`definitions()` の並びは別で、「どれにジャンプしたいか」の優先度:
型（Class/Interface/Type） → 呼べるもの（Function/Method/Macro） →
Constant/Module → Field/Property/Heading。

## 5. インデックス構築

```rust
SymbolIndex::build_cancellable(
    repo_root: &Path,
    paths: &[String],
    cancel: &dyn CancelSignal,
) -> IndexBuild
```

キャンセル不可の `build` は存在しない。戻り値も `SymbolIndex` ではなく `IndexBuild` で、
呼び手が 3 通りを描き分けられるようにしてある。

| バリアント | 意味 | 画面 |
|---|---|---|
| `Completed(SymbolIndex)` | 索引対象のパスを全部歩いた | `IndexState::Ready` |
| `Cancelled { scanned_files }` | 途中で signal が立った（新しい build に追い越された） | 何も描かない。チャネルへ何も送らない |
| `Failed { message }` | そもそも走れなかった。repo root が消えた／ワーカーが panic した | エラーバナー |

`Failed` を持つのが要点で、ワーカーの panic を握り潰して「シンボルが少ないインデックス」
として成功扱いすることを避けている。

`cancel` が `tokio_util::sync::CancellationToken` ではなく `&dyn CancelSignal` なのは、
**ポーリングの粒度をテストできるようにするため**である。「N 回ポーリングされたら立つ」
テスト用シグナルを渡せば、キャンセルされた build が何ファイル触るかを実時間に依存せず
確定できる。`CancellationToken` にはこのトレイトの実装が用意してある。

- ブロッキング CPU バウンド。**必ず `spawn_blocking` から呼ぶ**（描画ループから呼ばない）
- 対象は `supports_symbols()` が真で、`MAX_INDEXED_FILE_BYTES = 2 MiB` 以下の通常ファイルのみ
- ポーリング地点は 2 つ。metadata prefilter が `PREFILTER_CANCEL_POLL_INTERVAL` ごとに、
  各ワーカーが `index_chunk` の中で。前者で止まると `scanned_files` は 0
- ワーカー数は `available_parallelism().clamp(1, 8)` をさらに索引対象ファイル数で
  抑えた値。`std::thread::scope` で分割し、ワーカーごとに `ParserPool` を 1 つ持つ
  （パーサとコンパイル済みクエリの再利用のため）
- チャンクの完了順は不定なのでパス順にソートし直す。スナップショットと検索結果を
  決定的にするため

rayon などの並列ランタイムは追加していない。`thread::scope` で足りる。

## 6. 実測値

**この表に載せるのは `benches/symbol_index.rs` で再現できる値だけにすること。** 以前は
「octorus 自身を 1 回計測した」アドホックな数値が載っていたが、再現手順がないまま
`search` の実装が全ソートから top-N 部分選択 + メモ化へ変わり、誰も追随できなかった。

合成インデックス（5,000 ファイル × 20 シンボル = 100,000 シンボル、release ビルド、
この環境で `--warm-up-time 1 --measurement-time 3` で計測。Criterion の中央推定）:

| 操作 | 実測 |
|------|------|
| `from_files`（5,000 ファイル / 100,000 シンボル） | 8.48 ms |
| `definitions` ヒット | 39.6 ns |
| `definitions` ミス | 27.0 ns |
| `search_cached`（`h` / `handle` / `hrq`） | 462 / 462 / 465 ns |
| `search_cached`（`handle_request_2500`） | 290 ns |
| `search_cold`（`h` / `handle` / `hrq`） | 1.66 / 1.73 / 3.17 ms |
| `search_cold`（`handle_request_2500`） | 6.99 ms |

**`search_cached` と `search_cold` は別物なので混同しないこと。** メモ化は
`(needle, limit)` で効くので、同じクエリを繰り返す `b.iter` はスキャンではなく
「キャッシュ済み 200 件を `SymbolRef` へ復元するコスト」を測る。両方に意味がある —
オーバーレイは毎フレーム cached 経路を通り、cold 経路はキーストロークごとに 1 回だけ
通る。差は 3,500 倍あるので、名前を取り違えると回帰を丸ごと見落とす。`search_cold` は
limit を 1 ずつ交互に振ってメモを外している。

`handle_request_2500` が cold で最も遅いのは、needle が長いぶん 10 万シンボル全部に
対する `find` と部分列走査が重くなるためで、ヒット数の少なさでは埋め合わない。

### 自動ゲートで守られていない性質

- **`lowered_names` の事前計算**: `from_files()` が小文字名をキャッシュしているのは、
  `fuzzy_score_lowered` の中で毎回小文字化し直すと**候補 1 件につき String を 1 個確保**する
  ためである。10 万シンボルのインデックスならキーストロークごとに 10 万アロケーションになる。
  ところが**結果は完全に同一**なので、キャッシュを外してもテストは 1 本も落ちない
  （revert 掃討で実測）。大小無視のマッチ自体は
  `test_search_is_case_insensitive_in_both_directions` 系が固定しているが、守っているのは
  正しさであってコストではない（`test_search_is_case_insensitive_for_queries` が見ているのは
  マッチ結果である）。回帰を見るには `search_cold` ベンチを回すこと。

- **ワーカー panic がキャンセルより優先されること**: `build_cancellable` は `outcomes` を
  走査する途中で join エラーを見つけた時点で `IndexBuild::Failed` を `return` し、
  `stopped_early` の判定はループを抜けたあとにしかない。したがって panic は構造上つねに
  キャンセルに勝つ。この順序を入れ替えると、キャンセルを伴う panic が `Cancelled` として
  報告され、呼び手（`start_symbol_index_build` の空アーム）が捨てるためバナーが出ず
  panic が完全に握り潰される。**この順序を固定するテストはない** — 決定的に再現するには
  1 つのワーカーを panic させながら別のワーカーだけをキャンセルさせる必要があり、
  どちらのワーカーがどの poll を消費するかがスレッド順序依存になるため。
  並べ替えるときは自分で確かめること。

`benches/symbol_index.rs` に Criterion ベンチがある:

```bash
cargo bench --bench symbol_index
```

計測対象は 4 グループ:
- `extract_symbols_rust/{10,50,200,1000}` — ファイルサイズ別のスループット
- `extract_symbols_language/{rust,typescript,markdown}` — 言語別
- `from_files/{100,1000,5000}` — インデックス構築（100k シンボルまで）
- `query/{definitions_hit,definitions_miss,search_cached/*,search_cold/*}` — クエリ遅延

CI（`.github/workflows/benchmark.yml`）は現状 `ui_rendering` と `diff_parsing` しか
回していない。`symbol_index` を回帰監視に載せるならここに足す。

## 7. 言語を追加する手順

1. `Cargo.toml` に文法クレートを足し、`SupportedLanguage` にバリアントを追加
   （`from_extension` / `default_extension` / `ts_language` / `highlights_query` /
   `keywords` / `definition_prefixes` / `all()` をすべて埋める。コンパイラが漏れを教える）
2. クレートが `TAGS_QUERY` を export していれば `tags_query()` でそれを返す
3. していなければ `src/queries/<lang>/tags.scm` を書き、`include_str!` で埋め込む
   - ノード名は `Parser::parse()` した木を `to_sexp()` で出して確認するのが速い
   - `@definition.*` は `SymbolKind::from_capture()` が知っている名前を使う。
     未知の名前は**黙って捨てられる**（誤ったアイコンより表示しない方がマシという判断）
4. `test_all_tags_queries_compile` が通ることを確認（クエリのコンパイルエラーを検出する）
5. `test_languages_without_tags_query_are_intentional` の期待値を更新
6. 抽出結果のインラインスナップショットテストを 1 本足す

## 8. 既存 `src/symbol.rs` との関係

`src/symbol.rs` は削除していない。役割が違う:

| | `src/symbol.rs`（既存） | `src/symbols.rs`（新規） |
|---|---|---|
| 手法 | 定義キーワード前方一致 + `rg`/`grep` | CST 上の tags クエリ |
| 対象 | PR の patch 内 / リポジトリ grep | インデックス済みリポジトリ全体 |
| 使用箇所 | diff view の `gd` | Repo Browse の `gd` / `o` / `s` |
| 誤検出 | コメント・文字列中の同名トークンに当たる | 当たらない |
| 準備 | 不要（即時） | インデックス構築（バックグラウンド 数百 ms） |

将来的に diff view の `gd` もインデックス側へ寄せられるが、diff view は
「インデックスが無くても即座に動く」ことに価値があるので、置き換えは慎重に。
