# Using kcl from an Agent

`kcl` (kinetic context local) lets you query any registered codebase from within your agent session. Use it to look up how a dependency works, check API patterns in another project, or explore unfamiliar code — without leaving your current task.

## Prerequisites

- `kcl` is installed and on PATH
- At least one package is registered (`kcl list` to check)
- A harness is configured (`kcl config show` to verify)

## Core workflow

### 1. Check what packages are available

```bash
kcl list
```

This prints one identifier per line. Use `kcl list --json` for structured output.

### 2. Ask a question about a package

```bash
kcl ask <package> "your question here"
```

The response streams directly to stdout — treat it like output from any other CLI tool.

**Examples:**

```bash
# How does routing work in this framework?
kcl ask hono "How does the router middleware chain requests?"

# What's the schema for the users table?
kcl ask my-api "What columns are in the users table and what migrations created them?"

# Find usage patterns for a specific function
kcl ask axum "Show examples of how extractors are used in route handlers"
```

### 3. Use flags to control behavior

```bash
# Use a specific harness for this query
kcl ask hono "how does caching work?" --harness copilot

# Skip auto-pull (faster, uses cached code)
kcl ask hono "what middleware is available?" --no-pull

# Ask against a different branch — kcl checks it out, pulls it, runs the
# query, then restores whatever branch was checked out before
kcl ask hono "what changed on the next branch?" --branch next

# Include context from previous questions to avoid redundant exploration
kcl ask hono "what about error handling?" --context 3

# Increase timeout for complex questions
kcl ask hono "trace the full request lifecycle" --timeout 300
```

## Interpreting results

- **Exit code 0** — success, the harness answered the question
- **Exit code 1** — kcl error (bad identifier, config issue, etc.)
- **Exit code 2** — harness error (the underlying agent failed)

Check exit codes to decide whether to retry or adjust your question.

## Registering a new package on the fly

If you need to query a codebase that isn't registered yet:

```bash
# Register a git repo (kcl clones it automatically using the remote's default branch)
kcl packages add <name> --git <url>

# Pin to a specific branch at registration time
kcl packages add <name> --git <url> --branch <branch>

# Register a local directory
kcl packages add <name> --path /absolute/path/to/code
```

## Tips for agents

- **Be specific in your questions.** "How does X work?" gets better answers than "tell me about this codebase."
- **Use `--context` when asking follow-up questions** about the same package to avoid the harness re-exploring topics you've already covered.
- **Use `--no-pull`** when you're making multiple queries in quick succession — the code won't have changed between calls.
- **Use `--json` on read commands** (`kcl list --json`, `kcl packages show <id> --json`, `kcl history list <id> --json`) to get structured output you can parse reliably.
- **Check `kcl packages show <id>`** before asking questions to understand the package's source type, path, and configured harness.
