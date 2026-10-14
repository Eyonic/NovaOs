#!/usr/bin/env python3
"""Send pointer input to a QEMU VM over QMP (absolute coordinates), for tests.

Usage: qmp_input.py <qmp-socket> <screen-w> <screen-h> <action> [args...]
  click X Y        move to (X, Y) in screen pixels and click the left button
  move X Y         move only
  screendump FILE  save the screen as PNG
"""
import json
import socket
import sys
import time


def main():
    path, sw, sh, action, *args = sys.argv[1:]
    sw, sh = int(sw), int(sh)
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(path)
    f = s.makefile("rw")
    f.readline()  # greeting

    def cmd(execute, arguments=None):
        msg = {"execute": execute}
        if arguments is not None:
            msg["arguments"] = arguments
        f.write(json.dumps(msg) + "\n")
        f.flush()
        while True:
            reply = json.loads(f.readline())
            if "return" in reply or "error" in reply:
                if "error" in reply:
                    raise SystemExit(f"QMP error: {reply['error']}")
                return reply["return"]

    def move(x, y):
        ev = [{"type": "abs", "data": {"axis": "x", "value": int(x) * 32767 // (sw - 1)}},
              {"type": "abs", "data": {"axis": "y", "value": int(y) * 32767 // (sh - 1)}}]
        cmd("input-send-event", {"events": ev})

    def button(down):
        cmd("input-send-event", {"events": [{"type": "btn", "data": {"down": down, "button": "left"}}]})

    cmd("qmp_capabilities")
    if action == "click":
        move(*args)
        time.sleep(0.15)
        button(True)
        time.sleep(0.15)
        button(False)
    elif action == "move":
        move(*args)
    elif action == "screendump":
        cmd("screendump", {"filename": args[0], "format": "png"})
    else:
        raise SystemExit(f"unknown action {action}")


if __name__ == "__main__":
    main()
