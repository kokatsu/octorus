//! One-pass repository analysis for browser symbols and module dependencies.

use std::path::Path;

use hearth_graph::{AnalyzeBuild, BuildOptions, FsLoader};

use crate::module_graph::{ModuleGraph, SourceUniverse};
use crate::symbols::{symbol_language_registry, CancelSignal, SymbolIndex, MAX_INDEXED_FILE_BYTES};

/// Symbol and module indexes produced from one Hearth analysis pass.
#[derive(Debug)]
pub struct CodeIndex {
    pub symbols: SymbolIndex,
    pub modules: ModuleGraph,
}

/// Outcome of a cancellable combined repository analysis.
#[derive(Debug)]
pub enum CodeIndexBuild {
    Completed(Box<CodeIndex>),
    Cancelled { scanned_files: usize },
    Failed { message: String },
}

impl CodeIndex {
    /// Build both indexes without parsing a source file twice.
    ///
    /// Blocking and CPU-bound — call from `spawn_blocking`.
    pub fn build_cancellable(
        repo_root: &Path,
        paths: &[String],
        universe: SourceUniverse,
        cancel: &dyn CancelSignal,
    ) -> CodeIndexBuild {
        let loader = FsLoader::new(repo_root);
        let cancellation = || cancel.is_cancelled();
        let options = BuildOptions {
            max_file_bytes: MAX_INDEXED_FILE_BYTES,
            max_workers: 8,
        };

        match hearth_graph::analyze_paths(
            symbol_language_registry(),
            &loader,
            paths,
            &cancellation,
            &options,
        ) {
            AnalyzeBuild::Completed {
                mut files,
                scanned_files,
            } => {
                let Some(modules) =
                    ModuleGraph::from_analyses(repo_root, paths, &mut files, universe, cancel)
                else {
                    return CodeIndexBuild::Cancelled { scanned_files };
                };
                let Some(symbols) = SymbolIndex::from_analyses_cancellable(files, cancel) else {
                    return CodeIndexBuild::Cancelled { scanned_files };
                };
                CodeIndexBuild::Completed(Box::new(Self { symbols, modules }))
            }
            AnalyzeBuild::Cancelled { scanned_files } => {
                CodeIndexBuild::Cancelled { scanned_files }
            }
            AnalyzeBuild::Failed { message } => CodeIndexBuild::Failed { message },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::module_graph::{DependencyGuarantee, DependencyTarget};

    fn write(root: &Path, path: &str, source: &str) {
        let absolute = root.join(path);
        std::fs::create_dir_all(absolute.parent().unwrap()).unwrap();
        std::fs::write(absolute, source).unwrap();
    }

    fn completed(build: CodeIndexBuild) -> CodeIndex {
        match build {
            CodeIndexBuild::Completed(index) => *index,
            other => panic!("combined build did not complete: {other:?}"),
        }
    }

    #[test]
    fn test_combined_build_produces_symbols_and_import_edges() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "src/app.ts",
            "import { helper } from './helper';\nexport function app() { return helper(); }\n",
        );
        write(
            dir.path(),
            "src/helper.ts",
            "export function helper() { return 1; }\n",
        );
        let paths = vec!["src/app.ts".to_owned(), "src/helper.ts".to_owned()];

        let index = completed(CodeIndex::build_cancellable(
            dir.path(),
            &paths,
            SourceUniverse::Complete,
            &CancellationToken::new(),
        ));

        assert_eq!(index.symbols.definitions("app")[0].path, "src/app.ts");
        assert_eq!(index.symbols.definitions("helper")[0].path, "src/helper.ts");
        let deps = index.modules.dependencies("src/app.ts").unwrap();
        assert_eq!(deps.guarantee, DependencyGuarantee::Exact);
        assert_eq!(
            deps.edges[0].target,
            DependencyTarget::Path("src/helper.ts".into())
        );
    }

    #[test]
    fn test_combined_build_preserves_empty_symbol_files_for_import_queries() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/side_effect.ts", "import './target';\n");
        write(dir.path(), "src/target.ts", "// no symbols\n");
        let paths = vec!["src/side_effect.ts".to_owned(), "src/target.ts".to_owned()];

        let index = completed(CodeIndex::build_cancellable(
            dir.path(),
            &paths,
            SourceUniverse::Complete,
            &CancellationToken::new(),
        ));

        assert_eq!(
            index.symbols.file_symbols("src/target.ts"),
            Some([].as_slice())
        );
        assert_eq!(
            index
                .modules
                .dependencies("src/side_effect.ts")
                .unwrap()
                .edges
                .len(),
            1
        );
        assert!(index.modules.dependents("src/target.ts").is_some());
    }

    #[test]
    fn test_precancelled_combined_build_scans_nothing() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/app.ts", "export const app = 1;\n");
        let cancel = CancellationToken::new();
        cancel.cancel();

        match CodeIndex::build_cancellable(
            dir.path(),
            &["src/app.ts".to_owned()],
            SourceUniverse::Complete,
            &cancel,
        ) {
            CodeIndexBuild::Cancelled { scanned_files } => assert_eq!(scanned_files, 0),
            other => panic!("pre-cancelled build did not cancel: {other:?}"),
        }
    }

    struct PollCancel {
        limit: usize,
        polls: AtomicUsize,
    }

    impl CancelSignal for PollCancel {
        fn is_cancelled(&self) -> bool {
            self.polls.fetch_add(1, Ordering::SeqCst) >= self.limit
        }
    }

    #[test]
    fn test_combined_build_cancellation_never_publishes_partial_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<_> = (0..1_000)
            .map(|index| {
                let path = format!("src/file_{index:04}.ts");
                write(
                    dir.path(),
                    &path,
                    &format!("export const value{index} = {index};\n"),
                );
                path
            })
            .collect();
        let cancel = PollCancel {
            limit: 50,
            polls: AtomicUsize::new(0),
        };

        assert!(matches!(
            CodeIndex::build_cancellable(dir.path(), &paths, SourceUniverse::Complete, &cancel,),
            CodeIndexBuild::Cancelled { .. }
        ));
    }
}
