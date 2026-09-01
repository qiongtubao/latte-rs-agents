# latte-chat-driver

**Drive `latte-agent chat` interactively with auto-ask answering.**

This skill provides a Python script that spawns `latte-agent chat` as a subprocess, feeds a test scenario, monitors for `ask` prompts (which would otherwise hang waiting for stdin), and automatically selects an option so the test runs to completion without manual intervention.

---

## When to Use

- Testing `latte-agent chat -r manager` scenarios that trigger `ask` workflows (e.g., `design_and_plan`, `tdd_development`, decision points).
- Non-interactive CI/automation where you can't sit at the terminal to click an option.
- Any scenario where the manager calls `ask` and you want the test to proceed automatically.

---

## Installation

The skill lives in `.claude/skills/latte-chat-driver/`. No extra install needed — just make sure the driver script is executable:

```bash
chmod +x .claude/skills/latte-chat-driver/drive_chat.py
```

The script requires Python 3.8+ (uses asyncio subprocess).

---

## Usage

```bash
# Basic: run a scenario, pick first option (index 1)
python .claude/skills/latte-chat-driver/drive_chat.py -r manager "Your test prompt here"

# Pick option 2 (1-based)
python .claude/skills/latte-chat-driver/drive_chat.py -r manager --answer-index 2 "Test prompt"

# Custom timeout and extra args
python .claude/skills/latte-chat-driver/drive_chat.py -r manager -t 180 -- --tier premium "Test prompt"

# Point to a different latte-agent binary
LATTE_AGENT=/path/to/latte-agent python .claude/skills/latte-chat-driver/drive_chat.py -r manager "Test"
```

### Arguments

| Arg | Description | Default |
|-----|-------------|---------|
| `-r`, `--role` | Role to chat with | `manager` |
| `--answer-index` | Which option to pick (1-based) | `1` |
| `-t`, `--timeout` | Max seconds to wait | `120` |
| `--latte-agent` | Path to latte-agent binary | `./target/release/latte-agent` (env `LATTE_AGENT`) |
| `--extra-args` | Extra args passed to `latte-agent chat` | (none) |
| `scenario` (positional) | The prompt to send to chat | **required** |

---

## How It Works

1. **Spawns** `latte-agent chat -r <role> --debug --debug-format jsonl` as a subprocess with piped stdin/stdout/stderr.
2. **Feeds** the `scenario` text as the first user message (followed by newline).
3. **Monitors** stderr for the `ask` prompt marker (`❓ [role]`), then parses the numbered option lines (`1) label`, `2) label`, etc.) until the option block ends.
3. **Detects** the `answer ›` prompt on stdout (signaling it's time to answer).
4. **Writes** the chosen option index (default `1`) to the chat's stdin, satisfying the `ask` and letting the turn continue.
5. **Repeats** for any subsequent `ask` in the same session.
5. **Exits** when the chat process exits (after `/exit` or timeout).

### Ask Prompt Detection

The driver detects ask by scanning stderr for:
```
❓ [role] Your question here
  1) Option one label
     Description of option one
  2) Option two label
     Description of option two
```

It parses all `N) label` lines. The prompt ends when a non-option line appears. Then it sees `answer ›` on stdout and sends the chosen index.

### Answer Format

The chat expects a **1-based index** (e.g., `1`, `2`, or `1,3` for multi-select). The driver writes exactly that index followed by newline.

---

## Example: Full Automated Test

```bash
# Scenario: manager needs to decide between two architectures
python .claude/skills/latte-chat-driver/drive_chat.py -r manager \
  "We need a new caching layer. Option A: Redis. Option B: in-memory. Pick one."
# → ask fires, driver picks option 1 (Redis)

# Scenario: user must choose a task granularity before planning
python .claude/skills/latte-chat-driver/drive_chat.py -r manager \
  "Plan the login module. Choose task granularity: fine-grained (many small tasks) or coarse (few big tasks)."
# → ask fires, driver picks option 1 (fine-grained)
```

---

## Debugging

The driver prints every line from chat's stdout/stderr to stderr with a prefix:

```
[stdout] some model output
[stderr] ❓ [manager] Which architecture?
[stderr]   1) Microservices
[stderr]     Scalable, independent deploy
[stderr]   2) Modular monolith
[stderr]     Simpler ops
[driver] Detected ask with 2 options: ['Microservices', 'Modular monolith']
[driver] Answering ask with option 1: Microservices
```

Run with `RUST_LOG=debug` on the latte-agent binary for more detail:

```bash
RUST_LOG=debug python .claude/skills/latte-chat-driver/drive_chat.py -r manager "test"
```

---

## Limitations

- **Single answer per ask**: The driver answers once per ask. If the chat loops back asking again (invalid input), it will re-detect and answer again.
- **Option index only**: Currently supports numeric index answers (1, 2, ...). Free-text answers not implemented.
- **First match**: If multiple asks fire in quick succession, the driver handles them sequentially.
- **Stderr/stdout timing**: Relies on the chat's ordering (stderr for options → stdout for `answer ›`). Works for the current implementation.
- **No PTY**: Uses plain pipes; color codes may appear in captured output. The regex handles ANSI escape codes gracefully (optional enhancement).

---

## Extending

To add free-text answer support or multi-select, extend `ChatDriver._answer_ask()` in `drive_chat.py`. The `AskParser` in the same file handles option parsing — extend if option format changes.

---

## Integration with Claude

To run a test from a Claude session:

```python
# In a Claude skill invocation or REPL
import subprocess
result = subprocess.run([
    "python", ".claude/skills/latte-chat-driver/drive_chat.py",
    "-r", "manager",
    "--answer-index", "1",
    "Test scenario prompt here"
], capture_output=True, text=True, timeout=60)
print(result.stdout)
print(result.stderr)
print("exit:", result.returncode)
```