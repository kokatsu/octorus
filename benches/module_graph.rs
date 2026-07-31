use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use octorus::code_index::{CodeIndex, CodeIndexBuild};
use octorus::module_graph::SourceUniverse;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

fn fixture(file_count: usize) -> (TempDir, Vec<String>) {
    let dir = tempfile::tempdir().expect("fixture directory");
    std::fs::create_dir_all(dir.path().join("src")).expect("source directory");
    let paths: Vec<_> = (0..file_count)
        .map(|index| format!("src/module_{index:04}.ts"))
        .collect();
    for (index, path) in paths.iter().enumerate() {
        let import = if index + 1 < file_count {
            format!("import './module_{:04}';\n", index + 1)
        } else {
            String::new()
        };
        std::fs::write(
            dir.path().join(path),
            format!("{import}export function symbol_{index:04}() {{}}\n"),
        )
        .expect("fixture source");
    }
    (dir, paths)
}

fn fan_in_fixture(importer_count: usize) -> (TempDir, Vec<String>) {
    let dir = tempfile::tempdir().expect("fixture directory");
    std::fs::create_dir_all(dir.path().join("src")).expect("source directory");
    let mut paths = Vec::with_capacity(importer_count + 1);
    paths.push("src/center.ts".to_string());
    std::fs::write(
        dir.path().join("src/center.ts"),
        "export const center = 1;\n",
    )
    .expect("center source");
    for index in 0..importer_count {
        let path = format!("src/importer_{index:04}.ts");
        std::fs::write(
            dir.path().join(&path),
            format!("import './center';\nexport const value{index} = {index};\n"),
        )
        .expect("importer source");
        paths.push(path);
    }
    (dir, paths)
}

fn completed(build: CodeIndexBuild) -> CodeIndex {
    match build {
        CodeIndexBuild::Completed(index) => *index,
        CodeIndexBuild::Cancelled { scanned_files } => {
            panic!("fixture build cancelled after {scanned_files} files")
        }
        CodeIndexBuild::Failed { message } => panic!("fixture build failed: {message}"),
    }
}

fn module_graph_benches(c: &mut Criterion) {
    let mut build_group = c.benchmark_group("module_graph/build");
    build_group.sample_size(10);
    for file_count in [100, 1_000] {
        let (dir, paths) = fixture(file_count);
        build_group.bench_with_input(
            BenchmarkId::from_parameter(file_count),
            &file_count,
            |b, _| {
                b.iter(|| {
                    completed(CodeIndex::build_cancellable(
                        black_box(dir.path()),
                        black_box(&paths),
                        SourceUniverse::Complete,
                        &CancellationToken::new(),
                    ))
                });
            },
        );
    }
    build_group.finish();

    let (dir, paths) = fixture(5_000);
    let index = completed(CodeIndex::build_cancellable(
        dir.path(),
        &paths,
        SourceUniverse::Complete,
        &CancellationToken::new(),
    ));
    let mut query_group = c.benchmark_group("module_graph/query");
    query_group.bench_function("dependencies", |b| {
        b.iter(|| index.modules.dependencies(black_box("src/module_2500.ts")));
    });
    query_group.bench_function("dependents", |b| {
        b.iter(|| index.modules.dependents(black_box("src/module_2500.ts")));
    });

    let (fan_in_dir, fan_in_paths) = fan_in_fixture(5_000);
    let fan_in_index = completed(CodeIndex::build_cancellable(
        fan_in_dir.path(),
        &fan_in_paths,
        SourceUniverse::Complete,
        &CancellationToken::new(),
    ));
    query_group.bench_function("dependents_high_fan_in_5000", |b| {
        b.iter(|| fan_in_index.modules.dependents(black_box("src/center.ts")));
    });
    query_group.finish();
}

criterion_group!(benches, module_graph_benches);
criterion_main!(benches);
