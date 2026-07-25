# octorus を Repository Viewer へ — 技術調査と進化方針

> 対象読者: octorus のメンテナ。ここでは「なぜ Viewer なのか」「競合と何が違うのか」「どこまで実装したか」「次に何をやるか」を記録する。

## 1. 前提 — なぜ「エディタ」ではなく「ビューワ」なのか

コーディングエージェントが実装の大半を書くようになると、人間の作業時間の配分が変わる。

- **書く時間は減る。** 差分を生成するのはエージェントで、人間は指示と受け入れ判断をする。
- **読む時間は増える。** ただし読む対象が変わる。「自分が書いていないコード」「文脈を持たないコード」「今さっき生まれたばかりのコード」を、素早く把握する必要が出てくる。

エディタ（Zed / Helix / VS Code）は「書く」ための最適化 — LSP、補完、マルチカーソル、フォーマッタ連携 — に投資してきた。これらは書く時間が支配的だった時代の正しい設計だが、読む時間が支配的な作業には過剰であり、同時に不足している。過剰なのは編集機能一式と、それを支えるための起動コスト（LSP サーバの常駐、インデックス、設定）。不足しているのは「このコードはなぜここにあるのか」を辿る導線である。

octorus はすでに読む側に立っている。tree-sitter による 24 言語のハイライト、gh を抽象化したリポジトリ操作、diff / PR / issue / CI / git ops の統合。足りなかったのは **差分ではないコードを読む手段** だけだった。

## 2. 競合調査

### 2.1 エディタ (Zed, Helix, VS Code, Neovim)

| 観点 | エディタ | Viewer に求められるもの |
|------|---------|------------------------|
| コード知能 | LSP 前提。言語ごとにサーバ導入・設定が必要 | 何も入れずに即座に動くこと |
| 起動コスト | ワークスペース解析・インデックス構築 | 開いた瞬間に読めること |
| 編集 | 中核機能 | **不要**（むしろ誤編集のリスク） |
| PR / レビュー文脈 | 拡張機能で後付け（octo.nvim, GitHub PR 拡張など） | 一級市民であるべき |
| 「なぜこのコードがあるのか」 | blame は出るが、そこから PR・レビュー議論には繋がらない | ここが本命 |

差別化の要点は明確で、**エディタと同じ土俵（LSP・編集体験）で戦わないこと**。octorus は tree-sitter の文法をバイナリに同梱済みなので、LSP なしのコード知能を「導入コストゼロ」で提供できる。これは構造上エディタが真似しにくい（エディタは汎用性ゆえに LSP を捨てられない）。

### 2.2 ターミナル系ツール

| ツール | 守備範囲 | 欠けているもの |
|--------|---------|---------------|
| `tig` / `lazygit` / `gitui` | git 履歴・staging | 構文ハイライト付きのコード閲覧、シンボル移動、PR 文脈 |
| `bat` / `delta` | 単一ファイル / diff の綺麗な表示 | ナビゲーション、リポジトリ横断 |
| `gh` CLI | PR / issue の CRUD | 閲覧 UI |
| ripgrep / ast-grep | 検索 | 「読む」ための常設 UI |
| Claude Code / Codex / opencode 等のエージェント CLI | 生成・編集 | 人間が読むための画面 |

ターミナル上で「リポジトリ全体をコードとして読む」統合 UI は事実上空白地帯だった。

### 2.3 Web / SaaS 系

Sourcegraph, GitHub code search, CodeRabbit, Greptile, Devin review などは「意味的にコードを理解する」方向に投資している。ただし全てブラウザ or サービス側で完結し、**手元のワーキングツリー**（コミット前のエージェント出力）は見えない。octorus はローカルで動くので、ここが構造的な強みになる。

### 2.4 技術的な発見

`tree-sitter` の各文法クレートは、GitHub のコードナビゲーションが使っているのと同じ `tags.scm` を `TAGS_QUERY` として公開している。octorus が既に依存しているクレートのうち **12 言語がそのまま利用可能**（Rust / TS / JS / Go / Python / Ruby / C / C++ / Java / Lua / PHP / Swift）。C# / Zig / Bash / Haskell / MoonBit / Markdown は上流に無いので自前で `src/queries/<lang>/tags.scm` に同梱した。

つまり **新しい重量級の依存を一切増やさずに、LSP 相当のシンボル解決が手に入る**。これが今回の実装の土台になっている。

## 3. 進化方針 — 3 本柱

### Pillar A: Repo Browse（差分ではないコードを読む） — ✅ 実装済み

リポジトリ全体のファイルツリーと、任意ファイルの読み取り専用ビュー。

設計上の判断:

- ファイル一覧は `git ls-files --cached --others --exclude-standard`。`.gitignore` / submodule / sparse checkout を無料で正しく扱える。`--others` を含めるのは意図的で、**エージェントが今さっき作った未コミットのファイル**が見えないビューワは、いちばん必要なときに盲目になる。
- ファイル内容は「全行が context 行の擬似 patch」に変換して既存の `build_diff_cache` に通す。レンダリング経路を二重に持たないので、ハイライト・テーマ・injection の改善が自動的に両方へ効く。
- 読み取り専用。編集したくなったら `gf` で `$EDITOR` に渡す。

### Pillar B: Symbol Intelligence（LSP なしのコード知能） — ✅ 実装済み

`src/symbols.rs` に tree-sitter tags ベースのシンボルエンジンを新設。

- `extract_symbols()` — 単一ファイルのアウトライン（ネスト深さ付き、ソース順）
- `SymbolIndex` — リポジトリ全体のインデックス。バックグラウンドスレッドで構築し、UI は止めない
- `definitions()` — 完全一致（大小文字無視）。ジャンプ先として尤もらしい順に並べる
- `search()` — 完全一致 > 前方一致 > 単語境界 > 部分一致 > 部分列 の階層スコアリング

UI からは `o`（アウトライン）/ `s`（リポジトリ横断シンボル検索）/ `gd`（定義ジャンプ）/ `Ctrl-o`（戻る）。

既存の `src/symbol.rs`（`fn ` などのキーワード前方一致 + grep）との違いは決定的で、**コメントや文字列中の同名トークンに引っかからない**。CST 上の定義ノードだけを見ている。

実測（octorus 自身、162 ファイル / 約 70k LOC、デバッグではなく release ビルド）:

| 操作 | 実測値 |
|------|--------|
| インデックス構築（121 ファイル / 3,439 シンボル） | 約 250 ms（バックグラウンド） |
| ファジー検索 1 回 | 約 0.4 ms |
| 定義の完全一致検索 | 約 1 µs |

`benches/symbol_index.rs` に Criterion ベンチを追加済み（抽出のファイルサイズ別・言語別、インデックス構築、クエリ遅延）。

### Pillar C: Code Archaeology（octorus 固有の強み） — 🔜 未実装 / 次の一手

**ここが本命の差別化。** 「この行はなぜ存在するのか」を、行 → commit → PR → レビュー議論 と辿る導線。

octorus は現時点で、この経路に必要な材料を**すべて手元に持っている唯一のツール**である:

- ローカル git（blame / log / show）— Git Ops で既に使用中
- GitHub の PR・レビューコメント・議論 — PR ビューで既に取得済み
- tree-sitter のシンボル境界 — Pillar B で追加

Zed も Helix も blame は出せるが、そこから「その変更を承認したレビューで何が議論されたか」までは繋がらない。GitHub の Web UI は逆に、手元のワーキングツリーが見えない。

想定する体験:

1. Repo Browse でファイルを開き、任意の行にカーソルを置く
2. `gb` で blame オーバーレイ — 行ごとに commit / 著者 / 日付
3. `Enter` でその commit の diff（`git_ops` の commit diff ビューを再利用）
4. commit message / `gh api` から PR 番号を解決し、既存の PR ビュー・コメントリストへジャンプ
5. その行に紐づくレビューコメントがあれば直接表示

実装コスト見積り: blame 取得と行→commit マップは小さい（`git blame --porcelain` のパース）。commit → PR の解決は `gh api repos/{repo}/commits/{sha}/pulls` で 1 リクエスト。既存の PR / コメント表示にそのまま合流できるので、UI の新規作成はオーバーレイ 1 枚で済む。

### Pillar C の先にあるもの（ラフスケッチ）

- **References（`gr`）** — tags.scm には `@reference.call` / `@reference.class` も含まれている。定義側と同じ仕組みで参照検索が作れる。
- **Session diff** — 「前回このファイルを見たとき」からの差分。vibe coding では「エージェントが自分の不在中に何を変えたか」が最も知りたい差分になる。
- **Symbol-level review** — 行ではなく関数単位でのレビュー対象マーキング（`v` の symbol 版）。6,000 ファイルの PR を関数粒度で潰していく。

## 4. 今回の変更点

| 追加 | 内容 |
|------|------|
| `src/symbols.rs` | tree-sitter tags ベースのシンボル抽出とリポジトリインデックス |
| `src/queries/{c_sharp,zig,bash,haskell,moonbit,markdown}/tags.scm` | 上流に tags.scm が無い言語向けの同梱クエリ |
| `src/app/browse.rs` | Repository Browser の状態機械・ファイル読み込み・非同期インデックス |
| `src/app/input_browse.rs` | ブラウザのキー処理（2 ペイン + 2 オーバーレイ） |
| `src/ui/browse.rs` | ブラウザの描画 |
| `benches/symbol_index.rs` | シンボルエンジンの Criterion ベンチ |
| 変更 | `AppState` に `RepoBrowseTree` / `RepoBrowseFile`、Cockpit に `Repo Browse`、`--browse` フラグ、`repo_browse` / `symbol_outline` / `symbol_search` キーバインド |

状態は既存の設計原則どおり列挙型で表現している — 画面は `AppState`、ファイル一覧の読み込みは `LoadState`、インデックスは `IndexState`（`Idle` → `Building` → `Ready` / `Failed`）、オーバーレイは `BrowseOverlay`。真偽値フラグの追加は無い。
