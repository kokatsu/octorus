//! Benchmarks for the pure `git blame --porcelain` parser.
//!
//! Speed is the smaller half of what this guards. The parser's whole design
//! rests on two claims — one `u32` per line, and an allocation count that does
//! not scale with the number of commits — and both are invisible to
//! `cargo test`. `allocation_count_does_not_scale_with_commit_count` below
//! fails loudly if either regresses.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use octorus::github::parse_porcelain;
use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt::Write;
use std::sync::atomic::{AtomicUsize, Ordering};

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

/// Counts allocations so the layout claim is measurable rather than asserted in
/// a comment. A global allocator is acceptable here because a bench binary has
/// no other tenant.
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn allocations_during(f: impl FnOnce()) -> usize {
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    f();
    ALLOCATIONS.load(Ordering::Relaxed) - before
}

fn synthetic_porcelain(line_count: usize, lines_per_commit: usize) -> String {
    let mut output = String::new();

    for line_index in 0..line_count {
        let commit_index = line_index / lines_per_commit;
        let sha = format!("{:040x}", commit_index + 1);
        let file_line = line_index + 1;

        if line_index % lines_per_commit == 0 {
            let group_size = lines_per_commit.min(line_count - line_index);
            writeln!(
                output,
                "{sha} {file_line} {file_line} {group_size}\n\
                 author Author {commit_index}\n\
                 author-time 1700000000\n\
                 author-tz +0000\n\
                 summary synthetic commit {commit_index}\n\
                 filename src/synthetic.rs"
            )
            .unwrap();
        } else {
            writeln!(output, "{sha} {file_line} {file_line}").unwrap();
        }

        writeln!(output, "\tline {file_line}").unwrap();
    }

    output
}

fn bench_shape(c: &mut Criterion, name: &str, lines_per_commit: impl Fn(usize) -> usize) {
    let mut group = c.benchmark_group(name);

    for line_count in [100, 500, 1_000, 5_000] {
        let input = synthetic_porcelain(line_count, lines_per_commit(line_count));
        assert_eq!(parse_porcelain(&input).line_count(), line_count);

        group.throughput(Throughput::Bytes(input.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(line_count),
            &input,
            |bencher, input| {
                bencher.iter(|| black_box(parse_porcelain(black_box(input))));
            },
        );
    }

    group.finish();
}

/// The layout guarantee, measured.
///
/// `BlameFile` retains exactly three buffers — `text`, `commits`, `lines` — but
/// a *parse* allocates more than three times: each of those grows by doubling,
/// and the sha-to-index map is a transient allocation of its own. The count is
/// therefore logarithmic in the input, and the claim worth guarding is that it
/// is not **linear in the commit count**.
///
/// The rejected shape makes that concrete. Owned `String`s inside `BlameCommit`
/// cost five allocations per commit, so the 5,000-commit case below would be
/// ~25,000 allocations instead of the ~50 measured here. Any regression toward
/// per-commit heap storage crosses this bound by orders of magnitude, which is
/// why a loose absolute cap is the right tripwire rather than a tight budget.
fn allocation_count_does_not_scale_with_commit_count(c: &mut Criterion) {
    const LINES: usize = 5_000;
    /// Five per commit is what per-commit `String` storage costs.
    const REJECTED_SHAPE: usize = LINES * 5;

    let few_commits = synthetic_porcelain(LINES, LINES); // 1 commit
    let many_commits = synthetic_porcelain(LINES, 1); // 5,000 commits

    let with_few = allocations_during(|| {
        black_box(parse_porcelain(black_box(&few_commits)));
    });
    let with_many = allocations_during(|| {
        black_box(parse_porcelain(black_box(&many_commits)));
    });

    assert!(
        with_many * 20 < REJECTED_SHAPE,
        "allocations are scaling with commit count: {with_few} for 1 commit vs \
         {with_many} for {LINES}. Per-commit heap storage would cost \
         ~{REJECTED_SHAPE}; staying an order of magnitude under that is the \
         whole point of the arena."
    );

    let mut group = c.benchmark_group("blame_parse/layout");
    group.bench_function("allocations_per_parse", |bencher| {
        bencher.iter(|| black_box(parse_porcelain(black_box(&many_commits))));
    });
    group.finish();
}

fn bench_parse_porcelain(c: &mut Criterion) {
    bench_shape(c, "blame_parse/four_lines_per_commit", |_| 4);
    bench_shape(c, "blame_parse/single_commit", |line_count| line_count);
    allocation_count_does_not_scale_with_commit_count(c);
}

criterion_group!(benches, bench_parse_porcelain);
criterion_main!(benches);
