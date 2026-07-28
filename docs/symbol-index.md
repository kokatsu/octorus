# Symbol Index — Technical Reference

The details of the tree-sitter-tags-based symbol engine implemented in
`src/symbols.rs`. Read this when adding a language or adjusting queries.

## 1. What it does

It runs tree-sitter's `tags.scm` queries against the CST and extracts symbols
from pairs of `@definition.*` captures and `@name` captures. This is the same
query asset GitHub's code navigation ("Go to definition" / "Find all
references") uses.

```
source ──parse──> Tree ──Query(tags.scm)──> QueryMatch*
                                              │
                            ┌─────────────────┴──────────────────┐
                            │ @name           → name and position │
                            │ @definition.xxx → kind and range    │
                            └─────────────────┬──────────────────┘
                                              ↓
                              collapse_duplicate_tags()   ← dedupe per name node
                                              ↓
                              sort (start_byte asc, end_byte desc)
                                              ↓
                              containment stack computes depth
                                              ↓
                                        Vec<Symbol>
```

`Symbol` is `{ name, kind, line (1-based), column (0-based chars), depth }`.

## 2. Where each language's tags query comes from

Returned by `SupportedLanguage::tags_query()` (`src/language.rs`).

| Language | Source | Notes |
|----------|--------|-------|
| Rust | `tree_sitter_rust::TAGS_QUERY` | 14 patterns |
| TypeScript / TSX | **JS + TS concatenated** (`TYPESCRIPT_COMBINED_TAGS_QUERY`) | pitfall 2 below |
| JavaScript / JSX | `tree_sitter_javascript::TAGS_QUERY` | 11 patterns |
| Go | `tree_sitter_go::TAGS_QUERY` | |
| Python | `tree_sitter_python::TAGS_QUERY` | |
| Ruby | `tree_sitter_ruby::TAGS_QUERY` | |
| C | `tree_sitter_c::TAGS_QUERY` | |
| C++ | `tree_sitter_cpp::TAGS_QUERY` | **Self-contained**. No concatenation with C needed (unlike highlights) |
| Java | `tree_sitter_java::TAGS_QUERY` | |
| Lua | `tree_sitter_lua::TAGS_QUERY` | |
| PHP | `tree_sitter_php::TAGS_QUERY` | |
| Swift | `tree_sitter_swift::TAGS_QUERY` | |
| C# | `src/queries/c_sharp/tags.scm` | the crate ships `queries/tags.scm` but exports no constant |
| Zig | `src/queries/zig/tags.scm` | no upstream tags.scm |
| Bash | `src/queries/bash/tags.scm` | no upstream tags.scm |
| Haskell | `src/queries/haskell/tags.scm` | no upstream tags.scm |
| MoonBit | `src/queries/moonbit/tags.scm` | vendored grammar |
| Markdown | `src/queries/markdown/tags.scm` | turns headings into an outline |
| Svelte / Vue / CSS / MarkdownInline | `None` | deliberate. An SFC's `<script>` belongs to the embedded language; CSS has no named entities |

The `None` set is pinned by the test
`test_languages_without_tags_query_are_intentional`. Update that test when you
add a language — it is the guard against "accidentally left unsupported".

## 3. Pitfalls hit during implementation (important)

### 3.1 `Query::new` accepts the tags.scm directives

`tags.scm` files contain directives specific to the `tree-sitter-tags` crate,
such as `#strip!` / `#select-adjacent!` / `#not-eq?`. `tree_sitter::Query::new`
accepts these as general predicates and does not fail to compile. Therefore
**no dependency on the `tree-sitter-tags` crate is needed** — a bare `Query` +
`QueryCursor` suffices.

(Nothing depends on whether text predicates like `#not-eq?` are evaluated at
runtime. If they are not, the only consequence is one extra symbol — nothing
breaks.)

### 3.2 TypeScript's TAGS_QUERY assumes it inherits JavaScript

`tree_sitter_typescript::TAGS_QUERY` contains only the TS-specific patterns
(`function_signature`, `interface_declaration`, `module`, etc.).
`class_declaration` and `function_declaration` live on the JavaScript side.

Without concatenation, `export class Widget {}` yields **not a single symbol**.
This has exactly the same structure as `TYPESCRIPT_COMBINED_QUERY` for
`HIGHLIGHTS_QUERY`, so `TYPESCRIPT_COMBINED_TAGS_QUERY` sits right next to it.

C++ is the opposite: its `tags.scm` is self-contained (it carries both
`class_specifier` and `struct_specifier` itself). Concatenating by analogy with
the `highlights` habit produces duplicates — beware.

### 3.3 Rust: the same node matches two patterns

`tree-sitter-rust`'s tags.scm matches a function inside an impl block twice.

```scm
(declaration_list (function_item name: (identifier) @name) @definition.method)
(function_item     name: (identifier) @name)                @definition.function
```

`@definition.method` and `@definition.function` land on **the same
`function_item` node**. Processed naively,

- the outline shows `new` twice
- the containment stack decides the node "contains itself" and depth splits into 0 and 1

The countermeasure is `collapse_duplicate_tags()`. It keeps exactly one entry
**keyed by the name node's byte offset**. Priority is the lexicographic order of
`(kind_specificity, width of the definition node)`: `Method` is more specific
than `Function`, and between equals the narrower range wins.

### 3.4 Rust: `impl Foo` does not appear in the outline

`(impl_item type: (type_identifier) @name !trait) @reference.implementation` —
upstream treats it as a **reference**, not a definition. So the heading of an
`impl` block never shows up in the outline; the methods inside are listed
directly. This is an upstream design decision and octorus does not override it
(to change it, swap in a custom query for Rust alone).

### 3.5 Markdown: headings are siblings, so nesting is lost

`atx_heading` nodes do not contain one another. The nesting lives in `section`.

```scm
; ✗ this puts everything at depth 0
(atx_heading heading_content: (inline) @name) @definition.heading

; ✓ capturing the section puts `##` beneath `#`
(section (atx_heading heading_content: (inline) @name)) @definition.heading
```

### 3.6 Zig: `const Foo = struct {}` is a variable_declaration

What looks like a type is expressed not as a type-declaration node but as a
variable declaration whose initializer is a container declaration.

```scm
(variable_declaration (identifier) @name (struct_declaration)) @definition.class
```

### 3.7 Bash: only top-level assignments are picked up

Capturing `variable_assignment` unconditionally floods the outline with
function-local variables. The query is anchored with `(program ...)` to
restrict it to the top level.

### 3.8 Haskell: one match per defining equation

A function defined by several equations (`f [] = ...` / `f (x:xs) = ...`)
matches once per equation. This is absorbed after depth computation by
**folding adjacent identical `(name, kind, depth)` entries into one**.
"Adjacent only" is the important part: a method of the same name in a different
impl block stays separate.

### 3.9 tree-sitter columns are byte offsets

`Node::start_position().column` returns a byte offset. octorus handles display
width and cursor positions entirely in characters, so `char_column()` converts.
It actually drifts with CJK identifiers, or when a line starts with a Japanese
comment.

## 4. Search scoring

`fuzzy_score_lowered(lowered_name, needle)`. It is **private**, and **both
arguments are assumed already lowercased**. On the needle side,
`LoweredNeedle` guarantees this at construction; on the name side,
`from_files()` passes the precomputed `lowered_names` (so a keystroke does not
lowercase every symbol one by one). There is no public API for scoring a single
name; scores are observed only as the ordering `search()` returns.

| Tier | Score | Example (needle = `parse`) |
|------|-------|---------------------------|
| Exact match | 10,000 − length | `parse` |
| Prefix match | 8,000 − length | `parse_line` |
| Substring after a word boundary | 6,000 − position − length | `do_parse` (right after `_` `-` `.` `:`) |
| Substring | 4,000 − position − length | `reparsed` |
| Subsequence | 2,000 − min(gap, 1,000) − length | `please_advance_rest_of_set` |

"Length" is `chars().count()` (not bytes). The 1,000 cap on the subsequence
tier's gap is what keeps the tiers separated — without it, a scattered
subsequence match could sink below the tier beneath it. Ties are broken by
"shorter name → path order → line number" (so the rendered order does not change
from run to run).

`search(query, limit)` scores every entry, then cuts to `limit` entries with a
**top-N partial selection via `select_nth_unstable_by`, not a full sort**,
before ordering them (the measurements justifying this live in a comment inside
the function). On top of that, the most recent `(needle, limit)` pair's result
is memoized, so re-issuing the same query with the same limit does not rescan.
**A different limit misses the memo** — adding extra call sites doesn't just
recompute, the returned sets themselves diverge. The UI's single call site is
`BrowseState::symbol_search_hits()`; rendering, selection clamping, and Enter
resolution all go through it.

The ordering of `definitions()` is separate — a priority of "which one do you
want to jump to": types (Class/Interface/Type) → callables
(Function/Method/Macro) → Constant/Module → Field/Property/Heading.

## 5. Index construction

```rust
SymbolIndex::build_cancellable(
    repo_root: &Path,
    paths: &[String],
    cancel: &dyn CancelSignal,
) -> IndexBuild
```

There is no non-cancellable `build`. The return type is `IndexBuild`, not
`SymbolIndex`, so the caller can render the three outcomes distinctly.

| Variant | Meaning | Screen |
|---|---|---|
| `Completed(SymbolIndex)` | every indexable path was walked | `IndexState::Ready` |
| `Cancelled { scanned_files }` | the signal fired mid-run (overtaken by a newer build) | draw nothing; send nothing on the channel |
| `Failed { message }` | it could not run at all — the repo root vanished / a worker panicked | error banner |

Having `Failed` is the point: it avoids swallowing a worker panic and passing
off "an index with fewer symbols" as success.

`cancel` is a `&dyn CancelSignal` rather than a
`tokio_util::sync::CancellationToken` **so that polling granularity is
testable**. Pass a test signal that fires "after N polls" and you can pin down
how many files a cancelled build touches without depending on wall-clock time.
`CancellationToken` has an implementation of the trait.

- Blocking, CPU-bound. **Always call it from `spawn_blocking`** (never from the draw loop)
- Eligible files: `supports_symbols()` is true, regular file, at most `MAX_INDEXED_FILE_BYTES = 2 MiB`
- Two polling sites: the metadata prefilter every `PREFILTER_CANCEL_POLL_INTERVAL`,
  and each worker inside `index_chunk`. Stopping at the former means `scanned_files` is 0
- Worker count is `available_parallelism().clamp(1, 8)`, further capped by the
  number of indexable files. Work is split under `std::thread::scope`, one
  `ParserPool` per worker (to reuse parsers and compiled queries)
- Chunks complete in arbitrary order, so results are re-sorted by path — to keep
  snapshots and search results deterministic

No parallel runtime like rayon was added. `thread::scope` is enough.

## 6. Measured numbers

**Only values reproducible via `benches/symbol_index.rs` go in this table.** It
used to carry ad-hoc numbers from "measured octorus itself once"; with no
reproduction procedure, the `search` implementation then changed from a full
sort to top-N partial selection + memoization and nobody could follow up.

Synthetic index (5,000 files × 20 symbols = 100,000 symbols, release build,
measured on this machine with `--warm-up-time 1 --measurement-time 3`,
Criterion median estimates):

| Operation | Measured |
|-----------|----------|
| `from_files` (5,000 files / 100,000 symbols) | 8.48 ms |
| `definitions` hit | 39.6 ns |
| `definitions` miss | 27.0 ns |
| `search_cached` (`h` / `handle` / `hrq`) | 462 / 462 / 465 ns |
| `search_cached` (`handle_request_2500`) | 290 ns |
| `search_cold` (`h` / `handle` / `hrq`) | 1.66 / 1.73 / 3.17 ms |
| `search_cold` (`handle_request_2500`) | 6.99 ms |

**`search_cached` and `search_cold` are different things — do not conflate
them.** Memoization keys on `(needle, limit)`, so a `b.iter` that repeats the
same query measures not a scan but "the cost of rehydrating the cached 200
entries into `SymbolRef`s". Both are meaningful — the overlay takes the cached
path every frame, and the cold path runs once per keystroke. The gap is
3,500×, so mixing up the names means missing a regression entirely.
`search_cold` defeats the memo by alternating the limit by 1.

`handle_request_2500` is the slowest cold case because a longer needle makes the
`find` and the subsequence walk over all 100k symbols heavier, and the smaller
hit count does not make up for it.

### Properties not protected by automated gates

- **The `lowered_names` precomputation**: `from_files()` caches the lowercased
  names because re-lowercasing inside `fuzzy_score_lowered` would **allocate one
  String per candidate**. On a 100k-symbol index that is 100k allocations per
  keystroke. But **the results are exactly identical**, so removing the cache
  fails not a single test (measured in a revert sweep). Case-insensitive
  matching itself is pinned by the
  `test_search_is_case_insensitive_in_both_directions` family, but what it
  guards is correctness, not cost (what
  `test_search_is_case_insensitive_for_queries` looks at is the match results).
  To see the regression, run the `search_cold` bench.

- **Worker panics taking precedence over cancellation**: `build_cancellable`
  `return`s `IndexBuild::Failed` the moment it finds a join error while walking
  `outcomes`, and the `stopped_early` check happens only after that loop.
  Structurally, a panic therefore always beats cancellation. Swap that order
  and a panic that coincides with cancellation gets reported as `Cancelled`,
  which the caller (the empty arm in `start_symbol_index_build`) throws away —
  no banner, the panic fully swallowed. **No test pins this order** — a
  deterministic reproduction would need one worker to panic while only another
  gets cancelled, and which worker consumes which poll is thread-schedule
  dependent. When reordering, verify it yourself.

`benches/symbol_index.rs` holds the Criterion benches:

```bash
cargo bench --bench symbol_index
```

Four measured groups:
- `extract_symbols_rust/{10,50,200,1000}` — throughput by file size
- `extract_symbols_language/{rust,typescript,markdown}` — by language
- `from_files/{100,1000,5000}` — index construction (up to 100k symbols)
- `query/{definitions_hit,definitions_miss,search_cached/*,search_cold/*}` — query latency

CI (`.github/workflows/benchmark.yml`) currently runs only `ui_rendering` and
`diff_parsing`. To put `symbol_index` under regression watch, add it there.

## 7. How to add a language

1. Add the grammar crate to `Cargo.toml` and a variant to `SupportedLanguage`
   (fill in all of `from_extension` / `default_extension` / `ts_language` /
   `highlights_query` / `keywords` / `definition_prefixes` / `all()`; the
   compiler reports what you missed)
2. If the crate exports a `TAGS_QUERY`, return it from `tags_query()`
3. If not, write `src/queries/<lang>/tags.scm` and embed it with `include_str!`
   - The fastest way to check node names is `Parser::parse()` and dump the tree with `to_sexp()`
   - Use `@definition.*` names that `SymbolKind::from_capture()` knows. Unknown
     names are **silently dropped** (the judgement being that showing nothing
     beats showing a wrong icon)
4. Confirm `test_all_tags_queries_compile` passes (it catches query compile errors)
5. Update the expectations of `test_languages_without_tags_query_are_intentional`
6. Add one inline snapshot test for the extraction results

## 8. Relationship to the existing `src/symbol.rs`

`src/symbol.rs` is not deleted. The roles differ:

| | `src/symbol.rs` (existing) | `src/symbols.rs` (new) |
|---|---|---|
| Technique | definition-keyword prefix match + `rg`/`grep` | tags queries over the CST |
| Scope | inside the PR's patch / repository grep | the whole indexed repository |
| Used by | `gd` in the diff view | `gd` / `o` / `s` in Repo Browse |
| False positives | hits same-named tokens in comments and strings | does not |
| Preparation | none (instant) | index build (background, hundreds of ms) |

The diff view's `gd` could eventually lean on the index too, but the diff view's
value is "works instantly with no index", so replace it with care.
