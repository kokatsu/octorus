# octorus design docs

Working notes for the Repository Viewer direction. These are written for the
maintainer and for whoever (human or agent) picks the work up next — they carry
the context that does not fit in commit messages.

| Document | What it holds |
|----------|---------------|
| [repository-viewer.md](repository-viewer.md) | Why Viewer instead of Editor, the competitive research behind that call, and the three-pillar plan |
| [symbol-index.md](symbol-index.md) | Technical reference for the tree-sitter tags symbol engine — language matrix, per-grammar quirks, how to add a language, performance |
| [repo-browse-architecture.md](repo-browse-architecture.md) | Architecture of the Repository Browser — state machine, module map, background tasks, extension points, known limitations |
| [roadmap/code-archaeology.md](roadmap/code-archaeology.md) | Detailed design for Pillar C (line → commit → PR → review discussion), ready to implement |
| [session-log.md](session-log.md) | What was decided and discovered while building Pillars A and B, including the dead ends |

Reading order for someone new to this work: `repository-viewer.md` →
`repo-browse-architecture.md` → `roadmap/code-archaeology.md`.
