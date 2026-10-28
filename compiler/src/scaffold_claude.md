# CLAUDE.md — Anode 1.3.0 Project Instructions

Read [`AGENTS.md`](./AGENTS.md) before writing or editing any `.an` files in this project.

## Quick Rules for AI Agents
1. **Never guess Rust syntax for `.an` files.** Read [`AGENTS.md`](./AGENTS.md) first, especially §2.2 *Critical Rules Discovered in Practice*.
2. **Always check code with the Anode compiler (milliseconds):**
   ```powershell
   anode check . --json
   ```
3. **Run tests and execute:**
   ```powershell
   anode test .
   anode run .
   ```
4. **Use 4-worker parallel synthesis (`??` holes + `anode verify . --campaign`)** when implementing multiple functions in parallel inside the same `.an` file.
5. **Reuse the standard library** (`cli`, `encoding`, `csv_jsonl`, `markdown`, `glob_regex`, `json`, `docgen`, `bench`) from `$ANODE_HOME/packages/` through `[dependencies]` in `Anode.toml` (AGENTS.md §1 and §8).
