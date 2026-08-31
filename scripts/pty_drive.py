#!/usr/bin/env python3
"""用真 pty 驱动 `latte-agent chat`，验证分离模式（split screen）的交互行为。

# 为什么不用 `script -q`

`script` 在管道关闭时会给 pty 送 EOF，会话在处理输入前就退出（日志里
会看到一个 `^D`）。raw-mode 的交互必须由测试端控制写入时机才能模拟人
敲键，所以这里自己开 pty master/slave。

# 为什么必须设 winsize

`pty.openpty()` 不设 winsize 时终端上报 **0×0**。被测程序拿不到宽度时
若钳到 1 列，输出会被折成"每字符一行"——`\\x1b[36m` 会被拆成 6 行。
（那正是 `sane_width()` 那条测试锁住的 bug。）

# 用法

    pty_drive.py --cwd <工作目录> [--role manager] [--timeout 60] \\
                 --step "4.0:/roles" --step "2.5:/quit"

`--step` 的格式是 `等待秒数:要输入的文本`（回车自动补）。输出（含全部
转义序列）原样打到 stdout，由调用方断言。
"""

import argparse
import fcntl
import os
import pty
import select
import struct
import subprocess
import sys
import termios
import time


def drive(binary, cwd, role, steps, timeout, rows=40, cols=120):
    """steps: [(等待秒数, 要写入的字节)]。返回 pty 上收到的全部输出。"""
    master, slave = pty.openpty()
    # 必须设 winsize：不设时终端报 0×0，被测程序拿不到真实宽度。
    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    proc = subprocess.Popen(
        [binary, "chat", "-r", role],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        cwd=cwd,
        close_fds=True,
    )
    os.close(slave)
    out = bytearray()
    deadline = time.time() + timeout

    def pump(secs):
        """读 `secs` 秒。返回 False 表示 pty 已关闭。"""
        end = time.time() + secs
        while time.time() < end:
            r, _, _ = select.select([master], [], [], 0.1)
            if not r:
                continue
            try:
                chunk = os.read(master, 65536)
            except OSError:
                return False
            if not chunk:
                return False
            out.extend(chunk)
        return True

    for wait, data in steps:
        if not pump(wait) or time.time() > deadline:
            break
        try:
            os.write(master, data)
        except OSError:
            break
    pump(3)
    try:
        proc.terminate()
        proc.wait(timeout=5)
    except Exception:
        proc.kill()
    os.close(master)
    return out.decode("utf-8", "replace")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", required=True)
    ap.add_argument("--cwd", required=True)
    ap.add_argument("--role", default="manager")
    ap.add_argument("--timeout", type=float, default=60.0)
    ap.add_argument(
        "--step",
        action="append",
        default=[],
        help=(
            "`等待秒数:文本`，回车自动补；文本以 `~` 开头则**不补回车**"
            "（模拟'打字但不提交'，用于验证 turn 期间输入行是否活着）。"
            "文本可为空（只等待）。"
        ),
    )
    args = ap.parse_args()

    steps = []
    for raw in args.step:
        wait, _, text = raw.partition(":")
        if not text:
            payload = b""
        elif text.startswith("~"):
            payload = text[1:].encode()   # 不补回车
        else:
            payload = (text + "\r").encode()
        steps.append((float(wait), payload))
    sys.stdout.write(drive(args.binary, args.cwd, args.role, steps, args.timeout))


if __name__ == "__main__":
    main()
