# stria

[![Crates.io](https://img.shields.io/crates/v/stria.svg)](https://crates.io/crates/stria)
[![CI](https://github.com/Reliary/stria/actions/workflows/ci.yml/badge.svg)](https://github.com/Reliary/stria/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)

A structural codebase indexer for LLM agents. Sub-millisecond queries, zero parsers, any language.

LLM agents routinely guess the wrong file paths because they lack structural context. Existing tools solve this by running hundreds of AST parsers and graph databases. They take minutes to index, consume hundreds of megabytes of RAM, and break on unsupported languages.

Stria takes a radically different approach. It uses grammar-free phrase extraction to index your entire codebase in milliseconds. It compiles to a 4.8MB static binary, uses a bundled SQLite database, requires zero configuration, and works on any language ever invented.

## Highlights

* **Lazy Context Expansion:** Stop blasting thousands of tokens into your LLM's context window. Stria gives agents a compact 50-token structural map of a file and lets them lazy-load full function bodies only when needed via the `expand_body` MCP tool.
* **Blazing Fast on Massive Repos:** Stria indexes the entire 3.1GB Linux kernel (72,000 files) from scratch in just **80 seconds**. On standard repositories, the build is near-instant (~0.16s) and queries execute in sub-milliseconds.
* **Zero Parsers:** There are no tree-sitter grammars to maintain. Stria parses C, Rust, Erlang, Nix, or Python exactly the same way, meaning it works out-of-the-box on legacy systems and esoteric languages.

## Table of Contents

* [Install](#install)
* [Quickstart](#quickstart)
* [Agent integration](#agent-integration)
* [Token efficiency](#token-efficiency)
* [MCP tools](#mcp-tools)
* [CLI usage](#cli-usage)
* [How it works](#how-it-works)
* [Benchmarks](#benchmarks)
* [Limits](#limits)

## Install

```bash
cargo install stria
```

You can also download a pre-built binary for macOS, Linux, or Windows from the [releases page](https://github.com/Reliary/stria/releases).

## Quickstart

```bash
cd my-project
stria setup --yes
stria serve
```

The index builds automatically on the first run. There is no init step, no config file, and no per-language setup.

## Agent integration

```bash
stria setup
```

This detects installed agents (OpenCode, Claude Code, Cursor, Windsurf) and adds a global Stria entry to their configuration files. You do not need to manually edit JSON. 

One `switch_repo` MCP tool lets the agent change projects mid-session without restarting the server.

When an agent starts a session, it calls `orient` first. The response includes the language breakdown, a tool guide with `use_when` hints, and workflow suggestions. The agent can then use:

1. **`code_search(task="...")`** to find the right file, its test, and the structural risk. Default tier paths are compact. Agents can add `expand_plan` for blast radius and coupled files.
2. **`who_calls(name="...")`** before refactoring a function to find every file that references it.
3. **`hidden_deps(file="...")`** to check whether a change reaches outside the current module.

Each tool returns structured JSON. The agent reads it directly.

## Token efficiency

Stria is built to maximize LLM context windows. Graph-based MCP servers often consume 3,000 to 8,000 tokens to answer a single codebase query. Stria compresses structural intelligence into tight, deterministic JSON payloads:

* **`orient`**: ~374 tokens
* **`code_search` (default)**: ~35 tokens
* **`code_search` (expanded)**: ~75 to 120 tokens
* **`search`**: ~100 tokens
* **`who_calls`**: ~50 tokens

By separating structure from content, Stria gives the agent exactly what it needs to know, right when it needs to know it, at a fraction of the cost.

## MCP tools

| Tool | What it does |
|---|---|
| `orient` | Repository manifest: module map, language breakdown, tool guide. |
| `code_search` | Find the file to edit, its test, and risk. Three expansion tiers. |
| `pre_edit` | Risk assessment: blast radius, verification candidates, coupled files. |
| `search` | Direct phrase-overlap search against the index. |
| `who_calls` | Find all files referencing a specific identifier. |
| `trace_callers` | N-hop caller chain. Depth 1 is direct, depth 2 finds indirect callers. |
| `hidden_deps` | Find files in different modules sharing rare vocabulary. |
| `expand_body` | Retrieve a full function body by its horizon hash. |
| `find_hash` | Look up a horizon hash by function name. |
| `health` | Index health: phrase count, file count, latency, and build time. |

## CLI usage

```
stria build --repo <path>     Build or rebuild the phrase index
stria search --repo <path>    Search the index from the terminal
stria serve --repo <path>     Start the MCP server (auto-builds if needed)
stria watch --repo <path>     Watch for file changes and rebuild automatically
```

## How it works

Stria reads all source files as raw text, splits them on delimiter boundaries, counts phrase frequency per file, and applies left-context entropy to classify definitions versus usage. 

Searches use IDF-weighted exact match for precision, with BM25 prefix and substring tiers for fuzzy matching. Each file gets optional multipliers for source path, test path, dependency path, and definition density. The index lives in a SQLite database at `.stria/phrases.sqlite`. Rebuilding is incremental, costing about 0.02s for unchanged files.

## Benchmarks

| Repository | Files | Build time | Query time | DB size |
|---|---|---|---|---|
| Small TypeScript | 258 | 0.16 s | 14 ms | 4 MB |
| Medium Go monolith | 998 | 0.89 s | 27 ms | 12 MB |
| **Linux kernel** | 72,000 | 80.0 s | 170 ms | 899 MB |

The Linux kernel is the ultimate stress test. It contains 72,000 files spanning C, assembly, Python, shell scripts, makefiles, device trees, and documentation. 

Stria indexes the entire 3.1GB repository from scratch in **80 seconds** on an Intel 155H. There is no AST parsing, no language configuration, and no memory bloat. It answers complex structural queries across the whole kernel in 170 milliseconds, running entirely from a 4.8MB static binary.

## Limits

* Ranking for massive definition files (like the Erlang standard library `gen_server.erl` at 1,278 phrases) hits a BM25 length normalization ceiling. The correct file is always in the index, but it may rank at 10 or 20 instead of 1.
* Stria is not a linter, bug finder, or security scanner. It measures vocabulary overlap, not code correctness.

## License

MIT
