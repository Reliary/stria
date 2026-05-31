# stria

Structural codebase indexer for LLMs. Sub-millisecond queries, zero parsers, any language.

```bash
cargo install stria
cd my-project
stria setup --yes       # auto-detect: OpenCode, Claude Code, Cursor, Windsurf
stria serve             # builds index in ~0.16s, starts MCP server
```

The index builds on first run -- no init step, no config files, no per-language setup. Point your agent at stria and call `orient` for a repo manifest in ~80 tokens.

## Agent onboarding

```bash
stria setup
```

Detects installed agents and adds a global stria entry. No manual JSON editing. One `switch_repo` MCP tool lets you change projects mid-session without restarting.

When the agent starts a session, it calls `orient` first. The response includes the repo's language breakdown, a tool guide with `use_when` hints for each tool, and workflow suggestions. Then:

1. **`code_search(task="...")`** to find the right file, its test, and risk. Default tier paths are compact. Add `expand_plan` for blast radius and coupled files.

2. **`who_calls(name="...")`** before refactoring a function or type to find every file that references it.

3. **`hidden_deps(file="...")`** to check whether a change reaches outside the current module.

Each tool returns JSON. The agent reads it directly -- no prose to parse.

## What it solves

Most code understanding tools need a parser (works for popular languages, breaks on everything else) or a full context dump (thousands of tokens per query). stria reads source files as raw text, splits on delimiters, and builds an inverted phrase-to-file index in seconds. No AST, no grammar files, no per-language configuration.

## MCP tools

| Tool | What it does |
|---|---|
| `orient` | Repo manifest: module map, language breakdown, tool guide (similar to 374 tokens) |
| `code_search` | Find the file to edit, its test, and risk. 3 expansion tiers (default, expand_plan, expand_full) |
| `pre_edit` | Risk assessment: blast radius, verification candidates, coupled files |
| `search` | Direct phrase-overlap search against the index. 1-5 keyword query |
| `who_calls` | All files referencing an identifier |
| `trace_callers` | N-hop caller chain. depth=1 is direct, depth=2 finds indirect callers |
| `hidden_deps` | Files in different modules sharing rare vocabulary (imports miss these) |
| `expand_body` | Retrieve a full function body by its horizon hash |
| `find_hash` | Look up a horizon hash by function name |

## CLI

```
stria build --repo <path>     Build or rebuild the phrase index
stria search --repo <path>    Search the index from the terminal
stria serve --repo <path>     Start the MCP server (auto-builds if needed)
stria watch --repo <path>     Watch for file changes and rebuild automatically
```

## How it works

Reads all source files, splits on delimiter boundaries, counts phrase frequency per file, applies left-context entropy to classify definitions versus usage. Searches use IDF-weighted exact match for precision, with BM25 prefix and substring tiers for fuzzy matching. Each file gets optional multipliers for source path, test path, dependency path, and definition density.

The index is a SQLite database at `.stria/phrases.sqlite`. The build is incremental: unchanged files cost approximately 0.02s per rebuild.

## Benchmarks

| Repo | Files | Build time | Query time | DB size |
|---|---|---|---|---|
| Small TypeScript | 258 | 0.16 s | 14 ms | 4 MB |
| Medium Go monolith | 998 | 0.89 s | 27 ms | 12 MB |
| Linux kernel | 72,000 | 80 s | 170 ms | 946 MB |

The kernel is the hard case. 72,000 files in C, assembly, Python, shell scripts, makefiles, device trees, and documentation. Every file is parsed the same way: as raw text. The index finishes in 80 seconds on a 4-core machine and answers queries at 170ms.

## Limits

- Ranking for long definition files (the Erlang standard library `gen_server.erl` at 1,278 phrases) hits BM25 length normalization. The right file is always in the index but may rank at 10-20 instead of 1.
- Not a linter, bug finder, or security scanner. It measures vocabulary overlap, not code correctness.

## Install

```bash
cargo install stria
```

Or download a pre-built binary from the [releases page](https://github.com/Reliary/stria/releases).

## License

MIT
