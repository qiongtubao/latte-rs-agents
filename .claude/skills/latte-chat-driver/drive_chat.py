#!/usr/bin/env python3
"""
Drive `latte-agent chat` with full PTY interaction (via pexpect).

Uses a real pseudo-terminal so `latte-agent chat` sees a TTY stdout,
enters interactive REPL mode, and blocks on `answer ›` when `ask` is called.
The driver then detects the prompt and lets you (the human/Claude) type the answer.

Usage:
    python drive_chat.py -r manager "scenario prompt"
    # interact with chat; when ask appears, type the option number (1, 2, ...)
    # then continue; type /exit when done.

    # Auto-answer mode: pre-supply one answer for every ask
    python drive_chat.py -r manager -a 1 "scenario"
"""

import argparse
import os
import re
import sys
import pexpect

LATTE_AGENT = os.environ.get("LATTE_AGENT", "./target/release/latte-agent")
# Prompt patterns chat uses in interactive REPL mode:
#   "› 👔 manager · MiniMax-M3 › " (role prompt, waiting for next user line)
#   "answer › " (ask tool blocking prompt)
ROLE_PROMPT_RE = re.compile(r"›\s*👔\s*manager\s*·\s*\w+\s*›\s*$")
ANSWER_PROMPT_RE = re.compile(r"answer\s*[›>]\s*$")

# Ask option lines look like:  "  1) Label" then optional "     Description"
OPTION_RE = re.compile(r"^\s*(\d+)\)\s+(.+)$")


class _StreamLogger:
    """File-like wrapper for pexpect.logfile_read (needs .write() method)."""
    def __init__(self, target):
        self._target = target

    def write(self, s):
        self._target.write(s)
        self._target.flush()

    def flush(self):
        self._target.flush()


class ChatDriver:
    """Drive latte-agent chat via PTY using pexpect."""

    def __init__(self, role: str, scenario: str, extra_args=None, answer=None):
        self.role = role
        self.scenario = scenario
        self.extra_args = extra_args or []
        # When set, answer every ask with this value (non-interactive).
        # When None, prompt the human on stderr for each answer.
        self.auto_answer = answer
        self.child = None
        self.ask_count = 0

    def run(self) -> int:
        args = [LATTE_AGENT, "chat", "-r", self.role] + self.extra_args
        print(f"[driver] Spawning: {' '.join(args)}", file=sys.stderr)

        self.child = pexpect.spawn(
            args[0],
            args[1:],
            encoding="utf-8",
            codec_errors="replace",
            timeout=None,
            echo=False,
        )
        # In encoding mode, logfile_read must be an object with .write(str) method
        self.child.logfile_read = _StreamLogger(sys.stderr)

        # Wait for the first role prompt (chat is ready)
        try:
            self.child.expect([ROLE_PROMPT_RE, pexpect.EOF, pexpect.TIMEOUT], timeout=60)
        except (pexpect.EOF, pexpect.TIMEOUT) as e:
            print(f"[driver] Initial prompt wait failed: {e}", file=sys.stderr)
            return 1

        # Send scenario as the first user message
        self.child.sendline(self.scenario)
        return self._interact()

    def _interact(self) -> int:
        """Main loop: watch for answer prompts, ask human for input."""
        while True:
            try:
                idx = self.child.expect(
                    [
                        ANSWER_PROMPT_RE,  # 0: ask is waiting
                        ROLE_PROMPT_RE,    # 1: normal prompt, waiting for next message
                        pexpect.EOF,       # 2: chat ended
                    ],
                    timeout=None,
                )
            except pexpect.EOF:
                print("[driver] Chat process ended (EOF)", file=sys.stderr)
                break
            except pexpect.TIMEOUT:
                print("[driver] No output for a long time (TIMEOUT). Ctrl-C to abort.", file=sys.stderr)
                continue
            except KeyboardInterrupt:
                print("\n[driver] Interrupted, sending Ctrl-C to chat", file=sys.stderr)
                self.child.sendintr()
                continue

            if idx == 0:
                self._handle_ask()
            elif idx == 1:
                self._handle_user_input()
            elif idx == 2:
                break

        try:
            self.child.wait()
        except Exception:
            pass
        return self.child.exitstatus or 0

    def _handle_ask(self):
        """An ask prompt is waiting. Parse options, get answer, send it."""
        self.ask_count += 1
        before = self.child.before or ""
        print(f"\n{'='*60}", file=sys.stderr)
        print(f"🤖 ASK #{self.ask_count} DETECTED — answer › is waiting", file=sys.stderr)

        # Parse numbered options from the buffer
        options = []
        for m in OPTION_RE.finditer(before):
            idx = int(m.group(1))
            label = m.group(2).strip()
            options.append((idx, label))

        if options:
            print("Options:", file=sys.stderr)
            for idx, label in options:
                print(f"  {idx}) {label}", file=sys.stderr)
        else:
            # Free-text ask, no numbered options — show recent context
            lines = [l for l in before.strip().split("\n") if l.strip()][-8:]
            print("Context (last lines before prompt):", file=sys.stderr)
            for l in lines:
                print(f"  {l}", file=sys.stderr)

        print("="*60, file=sys.stderr)

        if self.auto_answer is not None:
            answer = self.auto_answer
            print(f"[driver] Auto-answer: {answer}", file=sys.stderr)
        else:
            # Interactive: read answer from the driver's stdin
            print("👉 Type option number (e.g. 1) or free text, then Enter:", file=sys.stderr, end=" ")
            sys.stderr.flush()
            answer = sys.stdin.readline()
            if not answer:
                answer = "\n"
            answer = answer.rstrip("\n")
            if not answer:
                print("[driver] Empty answer, sending '1' as default", file=sys.stderr)
                answer = "1"

        print(f"[driver] Sending answer: {answer}", file=sys.stderr)
        self.child.sendline(answer)

    def _handle_user_input(self):
        """Normal prompt: chat waiting for the next user message."""
        if self.auto_answer is not None:
            # In auto mode, we don't drive the conversation beyond the scenario —
            # just stop here (the scenario is a one-shot test).
            print("[driver] Auto mode: ending session", file=sys.stderr)
            self.child.sendline("/exit")
            return

        print("\n[driver] Chat ready for next message. Type a message (or /exit to stop):", file=sys.stderr, end=" ")
        sys.stderr.flush()
        line = sys.stdin.readline()
        if not line:
            print("[driver] stdin closed, exiting", file=sys.stderr)
            self.child.sendline("/exit")
            return
        line = line.rstrip("\n")
        if line.strip() in ("quit", "exit", "/exit"):
            self.child.sendline("/exit")
            return
        self.child.sendline(line)


def main():
    parser = argparse.ArgumentParser(description="Drive latte-agent chat with interactive ask answering (PTY mode)")
    parser.add_argument("-r", "--role", default="manager", help="Role to use (default: manager)")
    parser.add_argument("-a", "--answer", default=None, help="Auto-answer for every ask (e.g. '1'). Without it, prompts on stderr")
    parser.add_argument("--latte-agent", help="Path to latte-agent binary", default=os.environ.get("LATTE_AGENT", "./target/release/latte-agent"))
    parser.add_argument("scenario", help="Initial scenario prompt")
    parser.add_argument("extra_args", nargs="*", help="Extra args to pass to latte-agent chat (after --)")
    args = parser.parse_args()

    global LATTE_AGENT
    if args.latte_agent:
        LATTE_AGENT = args.latte_agent

    if not os.path.exists(LATTE_AGENT):
        print(f"[driver] Error: latte-agent not found at {LATTE_AGENT}", file=sys.stderr)
        sys.exit(1)

    # Split extra args at --
    if "--" in args.extra_args:
        idx = args.extra_args.index("--")
        extra_args = args.extra_args[idx+1:]
    else:
        extra_args = args.extra_args

    driver = ChatDriver(
        role=args.role,
        scenario=args.scenario,
        extra_args=extra_args,
        answer=args.answer,
    )

    try:
        exit_code = driver.run()
    except KeyboardInterrupt:
        print("\n[driver] Interrupted", file=sys.stderr)
        sys.exit(130)

    sys.exit(exit_code)


if __name__ == "__main__":
    main()