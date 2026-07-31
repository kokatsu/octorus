# octorus design docs

Technical reference documents for the Repository Browser and its symbol engine,
written for maintainers and anyone who needs to change that code.

| Document | What it holds |
|----------|---------------|
| [symbol-index.md](symbol-index.md) | Technical reference for the tree-sitter tags symbol engine — language matrix, per-grammar quirks, how to add a language, performance |
| [module-graph.md](module-graph.md) | Import extraction, module resolution, graph guarantees, path projection, queries, and performance |
| [repo-browse-architecture.md](repo-browse-architecture.md) | Architecture of the Repository Browser — state machine, module map, background tasks, extension points, known limitations |

Start with `repo-browse-architecture.md` when changing the browser screen. Read
`symbol-index.md` for symbol extraction and `module-graph.md` for imports,
resolvers, or dependency queries.
