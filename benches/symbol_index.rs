//! Symbol index benchmarks for the Repository Browser.
//!
//! These measure the tree-sitter tags pipeline that backs the file outline,
//! workspace symbol search and Go to Definition:
//! - `extract_symbols` — per-file outline extraction across languages and sizes
//! - `SymbolIndex::from_files` — building the name lookup table
//! - `SymbolIndex::definitions` / `search` — query latency on a large index
//!
//! Index build time is what the user waits on when opening the browser, and
//! search latency is what they feel on every keystroke of a symbol query.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use octorus::symbols::{extract_symbols, FileSymbols, Symbol, SymbolIndex, SymbolKind};
use octorus::syntax::ParserPool;

/// Generate a Rust source file with `functions` top-level items.
fn rust_source(functions: usize) -> String {
    let mut source = String::with_capacity(functions * 80);
    source.push_str("use std::collections::HashMap;\n\n");
    for i in 0..functions {
        source.push_str(&format!(
            "pub struct Item{i} {{\n    pub name: String,\n}}\n\n\
             impl Item{i} {{\n    pub fn describe(&self) -> String {{\n        \
             format!(\"{{}}\", self.name)\n    }}\n}}\n\n\
             pub fn process_item_{i}(input: &str) -> Option<Item{i}> {{\n    \
             Some(Item{i} {{ name: input.to_string() }})\n}}\n\n"
        ));
    }
    source
}

/// Generate a TypeScript source file with `classes` classes.
fn typescript_source(classes: usize) -> String {
    let mut source = String::with_capacity(classes * 80);
    for i in 0..classes {
        source.push_str(&format!(
            "export interface Props{i} {{ id: number }}\n\
             export class Widget{i} {{\n  render(): void {{}}\n  update(): void {{}}\n}}\n\
             export function setup{i}() {{}}\n\n"
        ));
    }
    source
}

fn bench_extract_symbols_by_size(c: &mut Criterion) {
    let mut group = c.benchmark_group("symbol_index/extract_symbols_rust");

    for items in [10usize, 50, 200, 1000] {
        let source = rust_source(items);
        group.throughput(Throughput::Bytes(source.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(items), &source, |b, source| {
            let mut pool = ParserPool::new();
            b.iter(|| {
                black_box(extract_symbols(
                    black_box(source),
                    black_box("src/bench.rs"),
                    &mut pool,
                ))
            });
        });
    }

    group.finish();
}

fn bench_extract_symbols_by_language(c: &mut Criterion) {
    let mut group = c.benchmark_group("symbol_index/extract_symbols_language");

    let rust = rust_source(100);
    let typescript = typescript_source(100);
    let markdown: String = (0..300)
        .map(|i| format!("## Section {i}\n\nSome prose paragraph.\n\n"))
        .collect();

    let cases: [(&str, &str, &str); 3] = [
        ("rust", "src/bench.rs", &rust),
        ("typescript", "src/bench.ts", &typescript),
        ("markdown", "docs/bench.md", &markdown),
    ];

    for (name, filename, source) in cases {
        group.throughput(Throughput::Bytes(source.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(name),
            &(filename, source),
            |b, (filename, source)| {
                let mut pool = ParserPool::new();
                b.iter(|| {
                    black_box(extract_symbols(
                        black_box(source),
                        black_box(filename),
                        &mut pool,
                    ))
                });
            },
        );
    }

    group.finish();
}

/// A synthetic index shaped like a large repository.
fn synthetic_files(files: usize, symbols_per_file: usize) -> Vec<FileSymbols> {
    (0..files)
        .map(|file| FileSymbols {
            path: format!("src/module_{file}/component_{file}.rs"),
            symbols: (0..symbols_per_file)
                .map(|index| Symbol {
                    name: format!("handle_request_{file}_{index}"),
                    kind: if index % 3 == 0 {
                        SymbolKind::Class
                    } else {
                        SymbolKind::Function
                    },
                    line: index * 12 + 1,
                    column: 4,
                    depth: index % 3,
                })
                .collect(),
        })
        .collect()
}

fn bench_index_from_files(c: &mut Criterion) {
    let mut group = c.benchmark_group("symbol_index/from_files");

    for files in [100usize, 1000, 5000] {
        let symbols = synthetic_files(files, 20);
        group.throughput(Throughput::Elements((files * 20) as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(files),
            &symbols,
            |b, symbols| {
                b.iter(|| black_box(SymbolIndex::from_files(black_box(symbols.clone()))));
            },
        );
    }

    group.finish();
}

fn bench_index_queries(c: &mut Criterion) {
    // 5,000 files × 20 symbols = 100,000 symbols, the scale octorus targets.
    let index = SymbolIndex::from_files(synthetic_files(5000, 20));

    let mut group = c.benchmark_group("symbol_index/query");

    group.bench_function("definitions_hit", |b| {
        b.iter(|| black_box(index.definitions(black_box("handle_request_2500_10"))));
    });

    group.bench_function("definitions_miss", |b| {
        b.iter(|| black_box(index.definitions(black_box("no_such_symbol_anywhere"))));
    });

    for query in ["h", "handle", "handle_request_2500", "hrq"] {
        group.bench_with_input(BenchmarkId::new("search", query), query, |b, query| {
            b.iter(|| black_box(index.search(black_box(query), 200)));
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_extract_symbols_by_size,
    bench_extract_symbols_by_language,
    bench_index_from_files,
    bench_index_queries,
);
criterion_main!(benches);
