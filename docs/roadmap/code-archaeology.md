# Pillar C: Code Archaeology — 実装設計

> ステータス: **未実装**。Pillar A（Repo Browse）と B（Symbol Intelligence）の上に載る。
> このドキュメントはそのまま実装に入れる粒度で書いてある。

## 1. なぜこれが本命なのか

「この行はなぜ存在するのか」に答える導線 — **行 → commit → PR → レビュー議論**。

vibe coding では、目の前のコードを書いたのが自分ではないことが常態になる。
そのとき最も価値のある情報は構文でも型でもなく、**意図**である。意図は commit message と
PR の議論に残っている。

octorus はこの経路に必要な材料を**すべて手元に持っている唯一のツール**である:

| 材料 | 既存の実装箇所 |
|------|---------------|
| ローカル git（blame / log / show） | `src/github/commit.rs::fetch_local_commit_diff`、Git Ops 画面 |
| GitHub の PR・レビューコメント・議論 | `src/github/pr.rs`、`src/github/comment.rs`、Comment List 画面 |
| シンボル境界（関数単位の粒度） | `src/symbols.rs`（Pillar B で追加済み） |
| 任意ファイルの閲覧 UI | `src/ui/browse.rs`（Pillar A で追加済み） |

競合の状況:

- **Zed / Helix / VS Code** — blame は出せる。そこから「その変更を承認したレビューで
  何が議論されたか」へは繋がらない。PR は拡張機能の別世界にある
- **GitHub Web UI** — PR とレビューは持っているが、**手元のワーキングツリーが見えない**。
  エージェントが 30 秒前に書いた未コミットのコードは存在しないことになっている
- **Sourcegraph 等** — サービス側にインデックスがあり、ローカルの今の状態は見えない

つまりここは、octorus の既存資産の交差点にしかない領域である。

## 2. 想定する体験

```
Repo Browse でファイルを開く
   │
   ├─ gb          blame オーバーレイ ON/OFF
   │              gutter に  a1b2c3d  ushironoko  3 months ago  が出る
   │
   ├─ Enter       カーソル行の commit の diff を見る
   │              （Git Ops の commit diff ビューを再利用）
   │
   ├─ gp          その commit を含む PR へジャンプ
   │              → 既存の PR ビュー / Comment List にそのまま合流
   │
   └─ 行に紐づくレビューコメントがあれば blame gutter に ● を出す
```

## 3. 実装計画

### Phase C-1: blame オーバーレイ

**新規: `src/git_blame.rs`**

```rust
pub struct BlameLine {
    pub sha: String,          // 40 hex
    pub author: String,
    pub author_time: i64,     // unix epoch
    pub summary: String,      // commit message の 1 行目
    pub is_uncommitted: bool, // sha が全部 0 = 未コミット
}

/// `git blame --porcelain -- <path>` を実行して 1-based 行 → BlameLine を返す
pub async fn blame_file(repo_root: &Path, path: &str) -> Result<Vec<BlameLine>>;

/// porcelain 出力のパース（純関数、テストしやすい）
pub fn parse_porcelain(stdout: &str) -> Vec<BlameLine>;
```

`--porcelain` 出力の形式:

```
<sha> <orig-line> <final-line> <num-lines>      ← ヘッダ行（グループの先頭）
author Ushironoko
author-mail <...>
author-time 1712345678
author-tz +0900
summary fix: clamp description scroll
filename src/ui/pr_description.rs
\t<行の内容>                                     ← タブ始まりが実際のコード行
```

同じ commit の 2 回目以降は `<sha> <orig> <final> ` だけになり、
author などのヘッダが省略される。**sha → メタデータのマップを持ち回って埋める**のが
パーサの肝。

未コミット行は sha が `0000000000000000000000000000000000000000`。
`is_uncommitted` を立てて「Not Committed Yet」と表示する。エージェントが書いたばかりの
コードはここに入るので、**この分岐は主役であって例外ではない**。

エッジケース（テストを書く対象）:
- 空ファイル → 空 Vec
- 未追跡ファイル → `git blame` がエラー。オーバーレイを出さずフッタに理由を出す
- バイナリ → 同上
- 行数が `OpenFile::lines` と食い違う（外部で書き換わった）→ 短い方に合わせる
- CRLF

**状態**: `BrowseState` に追加。

```rust
pub enum BlameState {
    Off,
    Loading,
    Ready(Vec<BlameLine>),
    Failed(String),
}
```

`Option<Vec<...>>` + `bool` にはしない（原則 4）。

**描画**: `render_content` の gutter を拡張。`BlameState::Ready` のとき
`  a1b2c3d ushironoko 3mo ` を行番号の前に足す。相対時刻は既存の
`github::format_relative_time` を再利用できる。幅は端末幅に応じて縮める
（狭いときは sha だけ）。

**キー**: `blame_toggle` = `gb`（シーケンス。`gd` / `gf` / `gg` と同じ層で処理する）。

### Phase C-2: commit へジャンプ

blame gutter の sha から commit diff を開く。

再利用できるもの:
- `github::fetch_local_commit_diff(working_dir, sha)` — ローカル
- `github::fetch_commit_diff(repo, sha)` — GitHub
- `ui::diff_view::build_commit_diff_cache()` — commit diff 専用のキャッシュ構築
- Git Ops の commit diff ペイン（`AppState::GitOpsSplitDiff`）

いちばん素直なのは **Git Ops の commit diff ビューへ遷移して、戻り先を Repo Browse に
設定する**こと。新しい画面を作らずに済む。`GitOpsState::return_state` が既にあるので
そこに `AppState::RepoBrowseFile` を入れる。

### Phase C-3: commit → PR 解決

```
gh api repos/{owner}/{repo}/commits/{sha}/pulls
```

このエンドポイントは commit を含む PR の配列を返す（現在は GA、preview ヘッダ不要）。

**新規: `src/github/commit.rs` に追加**

```rust
pub struct CommitPr { pub number: u32, pub title: String, pub state: String }

pub async fn fetch_prs_for_commit(repo: &str, sha: &str) -> Result<Vec<CommitPr>>;
```

`gh_command(&["api", &endpoint])` を使う（既存のヘルパ）。

**フォールバック**: エンドポイントが空を返す場合（squash merge で sha が変わっている、
そもそも PR 経由でない、など）は commit message から `(#123)` を正規表現で拾う。
GitHub の squash merge 既定のタイトル形式なので実用上よく当たる。

**キャッシュ**: `SessionCache` に `sha → Vec<CommitPr>` を持たせる。blame すると同じ
commit が何十行にも出るので、キャッシュ無しだと同じリクエストを繰り返す。

**遷移**: PR 番号が取れたら既存の PR ビューへ。1 件なら直接、複数なら
`SymbolPopupState` 相当の選択ポップアップ。

### Phase C-4: 行に紐づくレビューコメント

PR のレビューコメントは `path` + `line`（および `original_line`）を持っている。
`src/app/comments.rs` に既にその対応付けのロジックがある。

blame で解決した PR のコメントを取得し、**現在開いているファイルのその行**に対応する
ものがあれば gutter に `●` を出す。`Enter` でコメントパネルを開く。

行番号のズレが最大の難所。PR 時点の行番号と現在の行番号は一致しない。
現実的な妥協案:

1. blame で「その行を最後に触った commit」が分かる
2. その commit を含む PR のコメントのうち、`original_line` が blame の
   `orig_line`（porcelain の 2 番目のフィールド）と一致するものを引く

`--porcelain` は元の行番号を返してくれるので、この対応付けは実は素直に取れる。
`BlameLine` に `orig_line: usize` を持たせておくこと。

## 4. 段階的に出荷する

C-1 だけでも単体で価値がある（blame は誰もが使う）。C-3 まで行くと octorus 固有の
体験になる。C-4 は完成形。**C-1 → C-2 → C-3 → C-4 の順に、それぞれ独立した PR に
できる**ように設計してある。

## 5. C の先（ラフスケッチ）

### References（`gr`）

`tags.scm` には `@reference.call` / `@reference.class` / `@reference.type` /
`@reference.implementation` も入っている。`src/symbols.rs` の `extract_symbols` は
今これらを捨てているが、`SymbolKind::from_capture` と同じ要領で
`ReferenceKind` を足せば参照インデックスが作れる。

注意: 参照は定義よりはるかに数が多い（octorus 自身で数万件規模になる）。
インデックスのメモリと構築時間を実測してから入れること。
名前の interning（`lasso::Rodeo`、既に依存にある）が効くはず。

### Session diff — 「留守中に何が変わったか」

vibe coding で最も知りたい差分は「前回このファイルを見たときからの差分」である。
エージェントが自分の不在中に何を変えたか。

実装案: セッション開始時の HEAD と、閲覧したファイルのハッシュを記録しておき、
`or` を開き直したときに「前回から変わったファイル」をツリーでマークする。
既存の local comments が使っている `~/.cache/octorus/` に置ける。

### Symbol-level review

6,000 ファイルの PR を行単位で潰すのは現実的でない。関数単位で「見た」を
マークできれば粒度が合う。`v`（mark viewed）のシンボル版。
`SymbolIndex` が関数の行範囲を持っているので、diff の行がどのシンボルに属するかは
引ける。

### AI Rally との接続

Repo Browse で読んでいるファイル・シンボルを AI Rally のコンテキストに渡す。
「今見ているこの関数について聞く」。プロンプトテンプレートの変数
（`{{diff}}` 等）に `{{focused_symbol}}` / `{{focused_file}}` を足す形。
