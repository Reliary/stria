# Contributing

## Commit format

```
type: short description
```

Types: `feat`, `fix`, `chore`, `docs`, `ci`, `refactor`, `test`.

Examples:
```
feat: add switch_repo MCP tool
fix: handle empty query in search
chore: bump rusqlite to 0.41.0
```

## PR process

1. Branch from master: `git checkout -b feature/my-change`
2. Make changes, run `pre-commit run --all-files` before committing
3. Push and open a PR against master
4. CI must pass (fmt, clippy, audit, deny, test, integration)
5. One review approval required

## Pre-commit hooks

```bash
pip install pre-commit     # or brew install pre-commit
pre-commit install
```

This runs `cargo fmt`, `cargo clippy`, and `gitleaks` on every commit.

## Security

- `cargo audit` checks dependencies for vulnerabilities (in CI)
- `gitleaks` checks for secrets (pre-commit + CI)
- `cargo deny` checks license compliance and duplicate deps (in CI)
- Never commit API keys, tokens, or credentials
