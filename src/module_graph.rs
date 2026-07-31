//! Compatibility facade over Hearth import analysis and module-graph queries.
//!
//! Graph keys remain repository-relative even though Hearth's filesystem
//! resolvers require absolute referrer paths. [`RootedResolver`] owns that
//! boundary so browser state never needs to know which path form a resolver
//! consumes.

use std::path::{Component, Path, PathBuf};

use hearth_graph::graph::{
    DepEdge as HearthDepEdge, DepsResult as HearthDepsResult, EdgeTargetOwned, Guarantee,
    ModuleGraph as HearthModuleGraph, NodeState,
};
use hearth_graph::{
    js_resolver, rust_resolver, FailedKind, FileAnalysis, ImportKind, JsResolveOptions, RawImport,
    ResolutionCompleteness, ResolutionOutcome, Resolve, Resolved, ResolverSet, RustResolveOptions,
    UnresolvedReason,
};
use rustc_hash::FxHashSet;

use crate::symbols::{symbol_language_registry, CancelSignal};

/// Whether the browser supplied every repository path to analysis.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SourceUniverse {
    Complete,
    #[default]
    Partial,
}

/// Accuracy attached to a dependency answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyGuarantee {
    Exact,
    Approximate,
}

impl From<Guarantee> for DependencyGuarantee {
    fn from(guarantee: Guarantee) -> Self {
        match guarantee {
            Guarantee::Exact => Self::Exact,
            Guarantee::Approximate => Self::Approximate,
        }
    }
}

/// Octorus-owned import syntax kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleImportKind {
    EsStatic,
    EsReexport,
    EsDynamic,
    CommonJs,
    TsImportRequire,
    RustUse,
    RustMod,
}

impl ModuleImportKind {
    /// Compact stable label used in dependency rows.
    pub fn label(self) -> &'static str {
        match self {
            Self::EsStatic => "import",
            Self::EsReexport => "re-export",
            Self::EsDynamic => "dynamic",
            Self::CommonJs => "require",
            Self::TsImportRequire => "import=",
            Self::RustUse => "use",
            Self::RustMod => "mod",
        }
    }
}

impl From<ImportKind> for ModuleImportKind {
    fn from(kind: ImportKind) -> Self {
        match kind {
            ImportKind::EsStatic => Self::EsStatic,
            ImportKind::EsReexport => Self::EsReexport,
            ImportKind::EsDynamic => Self::EsDynamic,
            ImportKind::CommonJs => Self::CommonJs,
            ImportKind::TsImportRequire => Self::TsImportRequire,
            ImportKind::RustUse => Self::RustUse,
            ImportKind::RustMod => Self::RustMod,
        }
    }
}

/// Resolved destination of one import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyTarget {
    Path(String),
    External(String),
    Unresolved(String),
}

impl DependencyTarget {
    /// One-line target text suitable for the browser overlay.
    pub fn label(&self) -> &str {
        match self {
            Self::Path(path) | Self::External(path) | Self::Unresolved(path) => path,
        }
    }
}

/// One module-graph edge projected into octorus-owned types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyEdge {
    pub from: String,
    pub target: DependencyTarget,
    pub specifier: String,
    pub kind: ModuleImportKind,
    pub line: usize,
    pub span: (usize, usize),
}

/// Direct dependencies or dependents of one repository file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyResult {
    pub edges: Vec<DependencyEdge>,
    pub guarantee: DependencyGuarantee,
}

/// Repository module graph backed by Hearth.
#[derive(Debug, Default)]
pub struct ModuleGraph {
    inner: HearthModuleGraph,
    /// Listed paths that are not analyzed graph nodes. Analyzed nodes are
    /// listed by construction; this exception set keeps JSON/CSS and skipped
    /// source targets navigable without duplicating every analyzed path.
    listed_non_analyzed_paths: FxHashSet<String>,
}

impl ModuleGraph {
    pub(crate) fn from_analyses(
        repo_root: &Path,
        paths: &[String],
        analyses: &mut [FileAnalysis],
        universe: SourceUniverse,
        cancel: &dyn CancelSignal,
    ) -> Option<Self> {
        if cancel.is_cancelled() {
            return None;
        }
        let root = absolute_root(repo_root);
        let resolvers = resolver_set(&root, paths, cancel)?;
        let analyzed_import_paths: FxHashSet<&str> = analyses
            .iter()
            .filter(|analysis| supports_imports(analysis.path.as_str()))
            .map(|analysis| analysis.path.as_str())
            .collect();
        let mut every_import_file_analyzed = true;
        let mut listed_non_analyzed_paths = FxHashSet::default();
        for (index, path) in paths.iter().enumerate() {
            if index.is_multiple_of(1_024) && cancel.is_cancelled() {
                return None;
            }
            if !analyzed_import_paths.contains(path.as_str()) {
                if supports_imports(path) {
                    every_import_file_analyzed = false;
                }
                listed_non_analyzed_paths.insert(path.clone());
            }
        }

        let mut inner = fold_analyses_into_graph(analyses, &resolvers, cancel)?;
        if cancel.is_cancelled() {
            return None;
        }
        inner.set_universe_complete(
            universe == SourceUniverse::Complete && every_import_file_analyzed,
        );
        Some(Self {
            inner,
            listed_non_analyzed_paths,
        })
    }

    #[must_use]
    pub fn dependencies(&self, path: &str) -> Option<DependencyResult> {
        self.is_analyzed(path)
            .then(|| self.inner.deps(path))
            .flatten()
            .map(project_result)
    }

    #[must_use]
    pub fn dependents(&self, path: &str) -> Option<DependencyResult> {
        self.is_analyzed(path)
            .then(|| self.inner.rdeps(path))
            .flatten()
            .map(project_result)
    }

    pub(crate) fn dependencies_bounded(
        &self,
        path: &str,
        limit: usize,
    ) -> Option<(DependencyResult, usize)> {
        self.is_analyzed(path)
            .then(|| self.inner.deps(path))
            .flatten()
            .map(|result| project_result_bounded(result, limit))
    }

    pub(crate) fn dependents_bounded(
        &self,
        path: &str,
        limit: usize,
    ) -> Option<(DependencyResult, usize)> {
        self.is_analyzed(path)
            .then(|| self.inner.rdeps(path))
            .flatten()
            .map(|result| project_result_bounded(result, limit))
    }

    #[must_use]
    pub(crate) fn is_listed(&self, path: &str) -> bool {
        self.is_analyzed(path) || self.listed_non_analyzed_paths.contains(path)
    }

    pub(crate) fn is_analyzed(&self, path: &str) -> bool {
        self.inner
            .node(path)
            .is_some_and(|node| matches!(node.state, NodeState::Analyzed { .. }))
    }

    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.inner.edge_count()
    }
}

#[must_use]
pub fn supports_imports(path: &str) -> bool {
    symbol_language_registry().supports_imports(Path::new(path))
}

fn fold_analyses_into_graph(
    analyses: &mut [FileAnalysis],
    resolvers: &ResolverSet,
    cancel: &dyn CancelSignal,
) -> Option<HearthModuleGraph> {
    let mut inner = HearthModuleGraph::new();
    for analysis in analyses
        .iter_mut()
        .filter(|analysis| supports_imports(analysis.path.as_str()))
    {
        if cancel.is_cancelled() {
            return None;
        }
        inner.upsert_file(analysis, resolvers, true);
        // Hearth copied each import into compact graph edges. Release the
        // analysis-side vectors before symbol projection to bound peak RSS.
        drop(std::mem::take(&mut analysis.imports));
    }
    Some(inner)
}

fn project_result(result: HearthDepsResult) -> DependencyResult {
    DependencyResult {
        edges: result.edges.into_iter().map(project_edge).collect(),
        guarantee: result.guarantee.into(),
    }
}

fn project_result_bounded(mut result: HearthDepsResult, limit: usize) -> (DependencyResult, usize) {
    let total = result.edges.len();
    result.edges.truncate(limit);
    (project_result(result), total)
}

fn project_edge(edge: HearthDepEdge) -> DependencyEdge {
    let target = match edge.to {
        EdgeTargetOwned::Path(path) => DependencyTarget::Path(path.to_string()),
        EdgeTargetOwned::External(package) => DependencyTarget::External(package.to_string()),
        EdgeTargetOwned::Unresolved(reason) => {
            DependencyTarget::Unresolved(unresolved_label(&reason))
        }
    };
    DependencyEdge {
        from: edge.from.to_string(),
        target,
        specifier: edge.specifier.to_string(),
        kind: edge.kind.into(),
        line: usize::try_from(edge.line)
            .expect("hearth-graph import line does not fit octorus usize"),
        span: (
            usize::try_from(edge.span.0)
                .expect("hearth-graph import span does not fit octorus usize"),
            usize::try_from(edge.span.1)
                .expect("hearth-graph import span does not fit octorus usize"),
        ),
    }
}

fn unresolved_label(reason: &UnresolvedReason) -> String {
    match reason {
        UnresolvedReason::NotFound => "not found".to_owned(),
        UnresolvedReason::Unsupported => "unsupported".to_owned(),
        UnresolvedReason::Failed { kind, detail } => {
            let kind = match kind {
                FailedKind::Config => "configuration error",
                FailedKind::Io => "I/O error",
                FailedKind::InvalidSpecifier => "invalid specifier",
                FailedKind::Other => "resolver error",
            };
            format!("{kind}: {detail}")
        }
    }
}

/// Adapts repository-relative graph keys to resolvers that require absolute paths.
struct RootedResolver {
    root: PathBuf,
    inner: Box<dyn Resolve>,
}

impl RootedResolver {
    fn new(root: &Path, inner: Box<dyn Resolve>) -> Self {
        Self {
            root: root.to_path_buf(),
            inner,
        }
    }
}

impl Resolve for RootedResolver {
    fn baseline_completeness(&self) -> ResolutionCompleteness {
        self.inner.baseline_completeness()
    }

    fn resolve(&self, from_file: &str, import: &RawImport) -> ResolutionOutcome {
        let from_path = Path::new(from_file);
        let absolute = if from_path.is_absolute() {
            from_path.to_path_buf()
        } else {
            self.root.join(from_path)
        };
        let Some(absolute) = absolute.to_str() else {
            return ResolutionOutcome {
                resolved: Resolved::Unresolved(UnresolvedReason::Failed {
                    kind: FailedKind::InvalidSpecifier,
                    detail: "repository path is not valid UTF-8".into(),
                }),
                dependencies: Vec::new(),
                notes: Vec::new(),
                completeness: ResolutionCompleteness::Partial,
            };
        };

        let mut outcome = self.inner.resolve(absolute, import);
        if let Resolved::Path(target) = &outcome.resolved {
            if let Some(relative) = relative_graph_path(&self.root, Path::new(target.as_str())) {
                outcome.resolved = Resolved::Path(relative.into());
            }
        }
        outcome
    }

    fn clear_cache(&self) {
        self.inner.clear_cache();
    }
}

fn resolver_set(root: &Path, paths: &[String], cancel: &dyn CancelSignal) -> Option<ResolverSet> {
    let tsconfig = ["tsconfig.json", "jsconfig.json"]
        .into_iter()
        .map(|name| root.join(name))
        .find(|path| path.is_file());
    let js = js_resolver(JsResolveOptions {
        tsconfig,
        ..JsResolveOptions::default()
    });
    let mut crate_roots = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        if index.is_multiple_of(1_024) && cancel.is_cancelled() {
            return None;
        }
        if is_explicit_rust_crate_root(path) {
            crate_roots.push(root.join(path).to_string_lossy().into_owned().into());
        }
    }
    let rust = rust_resolver(RustResolveOptions { crate_roots });

    Some(ResolverSet {
        js: Some(Box::new(RootedResolver::new(root, js))),
        rust: Some(Box::new(RootedResolver::new(root, rust))),
    })
}

fn is_explicit_rust_crate_root(path: &str) -> bool {
    path == "src/lib.rs"
        || path == "src/main.rs"
        || path.ends_with("/src/lib.rs")
        || path.ends_with("/src/main.rs")
}

fn absolute_root(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| {
        if root.is_absolute() {
            root.to_path_buf()
        } else {
            std::env::current_dir().map_or_else(|_| root.to_path_buf(), |cwd| cwd.join(root))
        }
    })
}

fn relative_graph_path(root: &Path, target: &Path) -> Option<String> {
    let relative = target.strip_prefix(root).ok()?;
    let mut result = String::new();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            continue;
        };
        if !result.is_empty() {
            result.push('/');
        }
        result.push_str(component.to_str()?);
    }
    (!result.is_empty()).then_some(result)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use hearth_graph::{AnalyzeBuild, BuildOptions, FsLoader};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::symbols::{symbol_language_registry, MAX_INDEXED_FILE_BYTES};

    fn write(root: &Path, path: &str, source: &str) {
        let absolute = root.join(path);
        std::fs::create_dir_all(absolute.parent().unwrap()).unwrap();
        std::fs::write(absolute, source).unwrap();
    }

    fn analyses(root: &Path, paths: &[String]) -> Vec<FileAnalysis> {
        let loader = FsLoader::new(root);
        match hearth_graph::analyze_paths(
            symbol_language_registry(),
            &loader,
            paths,
            &hearth_graph::NeverCancelled,
            &BuildOptions {
                max_file_bytes: MAX_INDEXED_FILE_BYTES,
                max_workers: 2,
            },
        ) {
            AnalyzeBuild::Completed { files, .. } => files,
            other => panic!("analysis did not complete: {other:?}"),
        }
    }

    fn graph(root: &Path, paths: &[&str], universe: SourceUniverse) -> ModuleGraph {
        let paths: Vec<_> = paths.iter().map(|path| (*path).to_owned()).collect();
        let mut analyses = analyses(root, &paths);
        ModuleGraph::from_analyses(
            root,
            &paths,
            &mut analyses,
            universe,
            &CancellationToken::new(),
        )
        .expect("graph build")
    }

    #[test]
    fn test_javascript_dependencies_preserve_kinds_targets_and_lines() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "src/app.ts",
            "import { value } from './value';\nexport { other } from './other';\nconst lazy = import('./lazy');\nconst common = require('./common');\nimport missing from './missing';\n",
        );
        for path in ["value", "other", "lazy", "common"] {
            write(
                dir.path(),
                &format!("src/{path}.ts"),
                &format!("export const {path} = 1;\n"),
            );
        }
        let graph = graph(
            dir.path(),
            &[
                "src/app.ts",
                "src/value.ts",
                "src/other.ts",
                "src/lazy.ts",
                "src/common.ts",
            ],
            SourceUniverse::Complete,
        );

        insta::assert_debug_snapshot!(graph.dependencies("src/app.ts"), @r#"
        Some(
            DependencyResult {
                edges: [
                    DependencyEdge {
                        from: "src/app.ts",
                        target: Path(
                            "src/value.ts",
                        ),
                        specifier: "./value",
                        kind: EsStatic,
                        line: 1,
                        span: (
                            22,
                            31,
                        ),
                    },
                    DependencyEdge {
                        from: "src/app.ts",
                        target: Path(
                            "src/other.ts",
                        ),
                        specifier: "./other",
                        kind: EsReexport,
                        line: 2,
                        span: (
                            55,
                            64,
                        ),
                    },
                    DependencyEdge {
                        from: "src/app.ts",
                        target: Path(
                            "src/lazy.ts",
                        ),
                        specifier: "./lazy",
                        kind: EsDynamic,
                        line: 3,
                        span: (
                            86,
                            94,
                        ),
                    },
                    DependencyEdge {
                        from: "src/app.ts",
                        target: Path(
                            "src/common.ts",
                        ),
                        specifier: "./common",
                        kind: CommonJs,
                        line: 4,
                        span: (
                            120,
                            130,
                        ),
                    },
                    DependencyEdge {
                        from: "src/app.ts",
                        target: Unresolved(
                            "not found",
                        ),
                        specifier: "./missing",
                        kind: EsStatic,
                        line: 5,
                        span: (
                            153,
                            164,
                        ),
                    },
                ],
                guarantee: Exact,
            },
        )
        "#);
    }

    #[test]
    fn test_listed_non_source_target_remains_navigable() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "src/app.ts",
            "import data from './データ.json';\nexport { data };\n",
        );
        write(dir.path(), "src/データ.json", "{\"value\":1}\n");
        let graph = graph(
            dir.path(),
            &["src/app.ts", "src/データ.json"],
            SourceUniverse::Complete,
        );

        let result = graph.dependencies("src/app.ts").unwrap();
        assert_eq!(
            result.edges[0].target,
            DependencyTarget::Path("src/データ.json".into())
        );
        assert!(graph.is_listed("src/データ.json"));
    }

    #[test]
    fn test_tsconfig_alias_and_external_package_resolution() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "tsconfig.json",
            r##"{"compilerOptions":{"baseUrl":".","paths":{"#db/*":["db/*"]}}}"##,
        );
        write(
            dir.path(),
            "src/app.ts",
            "import { db } from '#db/client';\nimport pkg from 'pkg';\n",
        );
        write(dir.path(), "db/client.ts", "export const db = 1;\n");
        write(
            dir.path(),
            "node_modules/pkg/package.json",
            r#"{"name":"pkg","main":"index.js"}"#,
        );
        write(
            dir.path(),
            "node_modules/pkg/index.js",
            "module.exports = 1;\n",
        );
        let graph = graph(
            dir.path(),
            &["src/app.ts", "db/client.ts"],
            SourceUniverse::Complete,
        );
        let result = graph.dependencies("src/app.ts").unwrap();

        assert_eq!(
            result.edges[0].target,
            DependencyTarget::Path("db/client.ts".into())
        );
        assert_eq!(
            result.edges[1].target,
            DependencyTarget::External("pkg".into())
        );
        assert_eq!(result.guarantee, DependencyGuarantee::Exact);
    }

    #[test]
    fn test_rust_dependencies_and_dependents_are_approximate() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "src/lib.rs",
            "mod child;\nuse crate::child::Thing;\n",
        );
        write(dir.path(), "src/child.rs", "pub struct Thing;\n");
        let graph = graph(
            dir.path(),
            &["src/lib.rs", "src/child.rs"],
            SourceUniverse::Complete,
        );

        let deps = graph.dependencies("src/lib.rs").unwrap();
        assert_eq!(deps.guarantee, DependencyGuarantee::Approximate);
        assert!(deps
            .edges
            .iter()
            .all(|edge| { edge.target == DependencyTarget::Path("src/child.rs".into()) }));
        let rdeps = graph.dependents("src/child.rs").unwrap();
        assert_eq!(rdeps.guarantee, DependencyGuarantee::Approximate);
        assert_eq!(rdeps.edges.len(), 2);
    }

    #[test]
    fn test_reverse_dependencies_require_a_complete_source_universe() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/a.ts", "import './b';\n");
        write(dir.path(), "src/b.ts", "export const b = 1;\n");
        write(dir.path(), "src/data.json", "{\"value\":1}\n");

        let exact = graph(
            dir.path(),
            &["src/a.ts", "src/b.ts", "src/data.json"],
            SourceUniverse::Complete,
        );
        assert_eq!(
            exact.dependents("src/b.ts").unwrap().guarantee,
            DependencyGuarantee::Exact
        );

        let partial = graph(
            dir.path(),
            &["src/a.ts", "src/b.ts"],
            SourceUniverse::Partial,
        );
        assert_eq!(
            partial.dependents("src/b.ts").unwrap().guarantee,
            DependencyGuarantee::Approximate
        );
    }

    #[test]
    fn test_high_fan_in_query_reports_total_while_bounding_projected_edges() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/center.ts", "export const center = 1;\n");
        let mut paths = vec!["src/center.ts".to_owned()];
        for index in 0..250 {
            let path = format!("src/importer_{index:03}.ts");
            write(
                dir.path(),
                &path,
                &format!("import './center';\nexport const value{index} = {index};\n"),
            );
            paths.push(path);
        }
        let mut analyses = analyses(dir.path(), &paths);
        let graph = ModuleGraph::from_analyses(
            dir.path(),
            &paths,
            &mut analyses,
            SourceUniverse::Complete,
            &CancellationToken::new(),
        )
        .expect("graph build");

        let (result, total) = graph.dependents_bounded("src/center.ts", 20).unwrap();
        assert_eq!(total, 250);
        assert_eq!(result.edges.len(), 20);
        assert_eq!(result.guarantee, DependencyGuarantee::Exact);
    }

    #[test]
    fn test_missing_import_supported_analysis_makes_rdeps_approximate() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/a.ts", "import './b';\n");
        write(dir.path(), "src/b.ts", "export const b = 1;\n");
        let oversized = "x".repeat(MAX_INDEXED_FILE_BYTES as usize + 1);
        write(dir.path(), "src/oversized.ts", &oversized);

        let graph = graph(
            dir.path(),
            &["src/a.ts", "src/b.ts", "src/oversized.ts"],
            SourceUniverse::Complete,
        );
        assert_eq!(
            graph.dependents("src/b.ts").unwrap().guarantee,
            DependencyGuarantee::Approximate
        );
    }

    #[test]
    fn test_unanalyzed_stub_does_not_claim_to_have_no_imports() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/app.ts", "import './oversized';\n");
        write(
            dir.path(),
            "src/oversized.ts",
            &"x".repeat(MAX_INDEXED_FILE_BYTES as usize + 1),
        );

        let graph = graph(
            dir.path(),
            &["src/app.ts", "src/oversized.ts"],
            SourceUniverse::Complete,
        );

        assert!(graph.dependencies("src/oversized.ts").is_none());
        assert!(graph.dependents("src/oversized.ts").is_none());
    }

    struct PollCancel {
        remaining: AtomicUsize,
    }

    impl CancelSignal for PollCancel {
        fn is_cancelled(&self) -> bool {
            self.remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_err()
        }
    }

    #[test]
    fn test_graph_fold_releases_analysis_import_vectors_after_copying_edges() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/app.ts", "import './target';\n");
        write(dir.path(), "src/target.ts", "export const target = 1;\n");
        let paths = vec!["src/app.ts".to_owned(), "src/target.ts".to_owned()];
        let mut analyses = analyses(dir.path(), &paths);
        assert!(analyses.iter().any(|analysis| !analysis.imports.is_empty()));

        let graph = ModuleGraph::from_analyses(
            dir.path(),
            &paths,
            &mut analyses,
            SourceUniverse::Complete,
            &CancellationToken::new(),
        )
        .expect("graph build");

        assert!(analyses.iter().all(|analysis| analysis.imports.is_empty()));
        assert_eq!(graph.dependencies("src/app.ts").unwrap().edges.len(), 1);
    }

    #[test]
    fn test_graph_fold_observes_cancellation_between_files() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/target.ts", "export const target = 1;\n");
        let mut paths: Vec<_> = (0..20)
            .map(|index| {
                let path = format!("src/file_{index}.ts");
                write(
                    dir.path(),
                    &path,
                    &format!("import './target';\nexport const value{index} = {index};\n"),
                );
                path
            })
            .collect();
        paths.push("src/target.ts".to_string());
        let mut analyses = analyses(dir.path(), &paths);
        let import_positions: Vec<_> = analyses
            .iter()
            .enumerate()
            .filter_map(|(index, analysis)| (!analysis.imports.is_empty()).then_some(index))
            .collect();
        assert!(import_positions.len() >= 2);
        let resolvers = resolver_set(dir.path(), &paths, &CancellationToken::new()).unwrap();
        let cancel = PollCancel {
            // Permit exactly one insertion, then cancel the next fold step.
            remaining: AtomicUsize::new(1),
        };

        assert!(fold_analyses_into_graph(&mut analyses, &resolvers, &cancel).is_none());
        assert!(analyses[import_positions[0]].imports.is_empty());
        assert!(!analyses[import_positions[1]].imports.is_empty());
    }

    #[test]
    fn test_import_support_matches_hearth_registry() {
        for path in ["src/lib.rs", "src/app.ts", "src/app.tsx", "src/app.js"] {
            assert!(supports_imports(path), "{path}");
        }
        for path in ["src/main.go", "README.md", "style.css"] {
            assert!(!supports_imports(path), "{path}");
        }
    }
}
