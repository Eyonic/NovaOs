#!/usr/bin/env python3
"""A scripted stand-in for the Anthropic Messages API, for Nova's automated tests.

It speaks the real wire protocol (chunked HTTP/1.1, server-sent events, thinking
blocks with signatures, tool_use with streamed partial JSON) and checks every
request Nova sends: required headers, request fields, that tool results answer
the previous tool calls, and that thinking blocks come back unchanged. A
violation is answered with HTTP 400 and logged, so the test sees an API error.

Scripted conversations (chosen by the first user message):
  * "...clock app..."      write_file -> check_app (PNG required) -> launch_app -> done
  * "...no Nova driver..." pci_config -> write_file (.rs) -> write_file (.md) -> done
  * "...list my files..."  run_command ls -> summary quoting the output
  * "...overloaded..."     first attempt 529, then a normal answer (retry path)
  * "...refuse..."         stop_reason refusal
  * "...open the test page..." open_url(http://10.0.2.2:<port>/test-page) -> done
It also serves /test-page: an HTML page that loads a well-known tracker, so the
browser test can check that Shields blocks it.
  * anything else           a short text answer

Usage: mock_anthropic.py <port> <log-file>
"""
import base64
import json
import os
import struct
import zlib
import re
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

VALID_KEY = "sk-ant-test-0123456789abcdef"
VALID_TOKEN = "nova-oauth-test-token"   # stands in for an `ant auth login` access token
DEEPSEEK_KEY = "sk-deepseek-test-0123456789"   # DeepSeek mode: Messages API under /anthropic, /models with Bearer


def authorized(headers):
    """API key in x-api-key, or an OAuth token as Bearer together with the oauth beta header."""
    headers = {k.lower(): v for k, v in headers.items()}
    if headers.get("x-api-key") == DEEPSEEK_KEY:
        return True
    if headers.get("authorization") == f"Bearer {VALID_TOKEN}":
        return "oauth-2025-04-20" in betas(headers) and "x-api-key" not in headers
    return headers.get("x-api-key") == VALID_KEY


def betas(headers):
    return [b.strip() for b in headers.get("anthropic-beta", "").split(",") if b.strip()]
LOG = None
LOCK = threading.Lock()
SAMPLE_RS = """// A small Rust program for the editor test.
use std::collections::HashMap;

/// Count how often each word appears.
fn word_counts(text: &str) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for word in text.split_whitespace() {
        *counts.entry(word.to_lowercase()).or_insert(0) += 1;
    }
    counts
}

fn main() {
    let text = "the quick brown fox jumps over the lazy dog";
    let counts = word_counts(text);
    let mut words: Vec<_> = counts.iter().collect();
    words.sort_by(|a, b| b.1.cmp(a.1)); /* most common first */
    for (word, n) in words.iter().take(3) {
        println!("{word}: {n}");
    }
    let total: usize = counts.values().sum();
    println!("{} words, {} distinct", total, counts.len());
}
"""
COMFY_JOBS = set()
SIGNATURES = {}  # thinking text -> signature we sent
ATTEMPTS = {}

CLOCK_APP = """// title: Clock
// icon: C
// color: #4A90E2
// size: 320x200

fn init() { #{ ticks: 0 } }

fn tick() { this.ticks += 1; }

fn draw(g) {
    g.clear(0x1A1B26);
    let t = now();
    let h = t.hour; let m = t.minute; let s = t.second;
    let pad = |v| if v < 10 { `0${v}` } else { `${v}` };
    g.round_rect(20, 20, g.width - 40, 110, 14, 0x262838);
    g.text_scaled(48, 50, `${pad.call(h)}:${pad.call(m)}:${pad.call(s)}`, 0xFFFFFF, 4);
    g.text(20, 150, "UTC", 0x9A9CB0);
    g.progress(20, 172, g.width - 40, 10, s * 100 / 59, 0x4A90E2);
}
"""


def log(msg):
    with LOCK:
        LOG.write(msg + "\n")
        LOG.flush()


def sse(event, data):
    return f"event: {event}\ndata: {json.dumps(data)}\n\n".encode()


class Stream:
    """Builds the SSE events of one assistant message."""

    def __init__(self, model):
        self.events = [sse("message_start", {"type": "message_start", "message": {
            "id": "msg_mock", "type": "message", "role": "assistant", "model": model, "content": [],
            "stop_reason": None, "usage": {"input_tokens": 1200, "output_tokens": 1,
                                           "cache_read_input_tokens": 900, "cache_creation_input_tokens": 0}}})]
        self.index = 0
        self.events.append(sse("ping", {"type": "ping"}))

    def thinking(self, text):
        sig = "sig_" + base64.b64encode(text.encode()).decode()[:24]
        SIGNATURES[text] = sig
        i = self.index
        self.events.append(sse("content_block_start", {"type": "content_block_start", "index": i,
                                                       "content_block": {"type": "thinking", "thinking": "", "signature": ""}}))
        for part in re.findall(r".{1,12}", text, re.S):
            self.events.append(sse("content_block_delta", {"type": "content_block_delta", "index": i,
                                                           "delta": {"type": "thinking_delta", "thinking": part}}))
        self.events.append(sse("content_block_delta", {"type": "content_block_delta", "index": i,
                                                       "delta": {"type": "signature_delta", "signature": sig}}))
        self.events.append(sse("content_block_stop", {"type": "content_block_stop", "index": i}))
        self.index += 1

    def text(self, text):
        i = self.index
        self.events.append(sse("content_block_start", {"type": "content_block_start", "index": i,
                                                       "content_block": {"type": "text", "text": ""}}))
        for part in re.findall(r".{1,7}", text, re.S):
            self.events.append(sse("content_block_delta", {"type": "content_block_delta", "index": i,
                                                           "delta": {"type": "text_delta", "text": part}}))
        self.events.append(sse("content_block_stop", {"type": "content_block_stop", "index": i}))
        self.index += 1

    def tool(self, tool_id, name, args):
        i = self.index
        self.events.append(sse("content_block_start", {"type": "content_block_start", "index": i,
                                                       "content_block": {"type": "tool_use", "id": tool_id, "name": name, "input": {}}}))
        raw = json.dumps(args)
        for k in range(0, len(raw), 40):
            self.events.append(sse("content_block_delta", {"type": "content_block_delta", "index": i,
                                                           "delta": {"type": "input_json_delta", "partial_json": raw[k:k + 40]}}))
        self.events.append(sse("content_block_stop", {"type": "content_block_stop", "index": i}))
        self.index += 1

    def finish(self, stop_reason, details=None):
        delta = {"stop_reason": stop_reason, "stop_sequence": None}
        if details:
            delta["stop_details"] = details
        self.events.append(sse("message_delta", {"type": "message_delta", "delta": delta, "usage": {"output_tokens": 321}}))
        self.events.append(sse("message_stop", {"type": "message_stop"}))
        return self.events


def validate(headers, body):
    problems = []
    if not authorized(headers):
        problems.append("bad credentials (x-api-key, or Bearer token without the oauth beta)")
    if headers.get("anthropic-version") != "2023-06-01":
        problems.append("anthropic-version must be 2023-06-01")
    if "application/json" not in headers.get("content-type", ""):
        problems.append("content-type must be application/json")
    model = body.get("model", "")
    deepseek = headers.get("x-api-key") == DEEPSEEK_KEY
    if deepseek:
        if model != "deepseek-flash":
            problems.append(f"DeepSeek model should be deepseek-flash (images), got {model!r}")
        if body.get("output_config", {}).get("effort") not in ("low", "high", "max"):
            problems.append("DeepSeek effort must be low, high or max")
        for m in body.get("messages", []):
            for b in m.get("content") if isinstance(m.get("content"), list) else []:
                inner = b.get("content") if b.get("type") == "tool_result" else None
                if isinstance(inner, list) and any(x.get("type") == "image" for x in inner):
                    problems.append("DeepSeek ignores images inside tool_result; send them beside it")
    elif model.startswith("deepseek"):
        problems.append("deepseek model sent with a non-DeepSeek key")
    if model.startswith("claude-opus-5") and "server-side-fallback-2026-07-01" not in betas(headers):
        problems.append("fallbacks need the server-side-fallback-2026-07-01 beta header")
    if body.get("fallbacks") not in (None, "default"):
        problems.append("fallbacks must be \"default\"")
    if body.get("stream") is not True:
        problems.append("stream must be true")
    if not isinstance(body.get("max_tokens"), int) or body["max_tokens"] < 1024:
        problems.append("max_tokens missing or too small")
    if body.get("thinking", {}).get("type") != "adaptive":
        problems.append("thinking must be adaptive")
    if "budget_tokens" in body.get("thinking", {}):
        problems.append("budget_tokens is rejected on this model")
    for t in body.get("tools", []):
        if "input_schema" in t and t.get("eager_input_streaming") is not True:
            problems.append(f"tool {t.get('name')} lacks eager_input_streaming")
    msgs = body.get("messages", [])
    if not msgs or msgs[0].get("role") != "user":
        problems.append("messages must start with a user turn")
    for a, b in zip(msgs, msgs[1:]):
        if a.get("role") == b.get("role"):
            problems.append("roles must alternate")
    pending = set()
    for m in msgs:
        content = m.get("content")
        blocks = content if isinstance(content, list) else []
        if m["role"] == "assistant":
            for b in blocks:
                if b.get("type") == "thinking":
                    want = SIGNATURES.get(b.get("thinking"))
                    if want is None or b.get("signature") != want:
                        problems.append("thinking block was not echoed unchanged")
            pending = {b["id"] for b in blocks if b.get("type") == "tool_use"}
        else:
            answered = {b.get("tool_use_id") for b in blocks if b.get("type") == "tool_result"}
            if pending and answered != pending:
                problems.append(f"tool results {sorted(answered)} do not answer tool calls {sorted(pending)}")
            pending = set()
    return problems


def first_user_text(msgs):
    c = msgs[0].get("content")
    if isinstance(c, str):
        return c
    return " ".join(b.get("text", "") for b in c if b.get("type") == "text")


def last_tool_result(msgs):
    last = msgs[-1]
    if last["role"] != "user" or not isinstance(last["content"], list):
        return None
    for b in last["content"]:
        if b.get("type") == "tool_result":
            return b
    return None


def result_text(result):
    c = result.get("content")
    if isinstance(c, str):
        return c
    return " ".join(b.get("text", "") for b in c if b.get("type") == "text")


def respond(body):
    msgs = body["messages"]
    model = body["model"]
    first = first_user_text(msgs)
    step = sum(1 for m in msgs if m["role"] == "assistant")
    result = last_tool_result(msgs)
    s = Stream(model)

    if "clock app" in first:
        if step == 0:
            s.thinking("The user wants a clock. I'll write a small Rhai app with a tick callback.")
            s.text("I'll build a **clock app** for your desktop.")
            s.tool("toolu_clock_1", "write_file", {"path": "apps/clock.rhai", "content": CLOCK_APP})
            return s.finish("tool_use")
        if step == 1:
            s.tool("toolu_clock_2", "check_app", {"path": "apps/clock.rhai", "events": ["tick 3"]})
            return s.finish("tool_use")
        if step == 2:
            blocks = result.get("content") if isinstance(result.get("content"), list) else []
            # DeepSeek mode sends the screenshot beside the tool result, in the same user turn.
            last = msgs[-1].get("content") if isinstance(msgs[-1].get("content"), list) else []
            images = [b for b in blocks + last if b.get("type") == "image"]
            ok = False
            if images:
                png = base64.b64decode(images[0]["source"]["data"])
                ok = png.startswith(b"\x89PNG\r\n\x1a\n") and b"IHDR" in png[:40]
            log(f"check_app result: {result_text(result)!r}, png ok: {ok}")
            if not ok or "without errors" not in result_text(result):
                s.text("The check failed: " + result_text(result))
                return s.finish("end_turn")
            s.tool("toolu_clock_3", "launch_app", {"path": "apps/clock.rhai"})
            return s.finish("tool_use")
        s.text("Your clock app is ready: it is open on the desktop and listed in the start menu.")
        return s.finish("end_turn")

    if "no Nova driver" in first:
        addr = re.search(r"- ([0-9a-f]{2}:[0-9a-f]{2}\.\d)", first)
        addr = addr.group(1) if addr else "00:00.0"
        if step == 0:
            s.thinking("An unsupported device. Inspect its configuration space first.")
            s.tool("toolu_drv_1", "pci_config", {"address": addr})
            return s.finish("tool_use")
        if step == 1:
            dump = result_text(result)
            log(f"pci_config: {dump.splitlines()[0] if dump else '(empty)'}")
            s.text("This is an Intel 82801AA AC'97 audio controller. Writing a driver port.")
            s.tool("toolu_drv_2", "write_file", {"path": "drivers/ac97.rs",
                                                 "content": "//! Intel AC'97 audio controller (draft port).\n"
                                                            "pub struct Ac97 { nambar: u16, nabmbar: u16 }\n"})
            s.tool("toolu_drv_3", "write_file", {"path": "drivers/ac97.md",
                                                 "content": "# Intel AC'97\nDraft: codec reset and PCM out buffer descriptor list.\n"})
            return s.finish("tool_use")
        s.text(f"Intel AC'97 ({addr}): draft driver in drivers/ac97.rs; needs an audio interface in the kernel.")
        return s.finish("end_turn")

    if "poster image" in first:
        if step == 0:
            s.text("I'll generate the artwork first.")
            s.tool("toolu_img_1", "generate_image", {"prompt": "a calm teal ocean at dusk, no text", "path": "apps/assets/poster-bg.png", "width": 256, "height": 144})
            return s.finish("tool_use")
        blocks = result.get("content") if isinstance(result.get("content"), list) else []
        last = msgs[-1].get("content") if isinstance(msgs[-1].get("content"), list) else []
        images = [b for b in blocks + last if b.get("type") == "image"]
        ok = bool(images) and base64.b64decode(images[0]["source"]["data"]).startswith(b"\x89PNG")
        log(f"generate_image result: {result_text(result)!r}, png ok: {ok}")
        s.text("Poster image ready." if ok and "Saved a 256x144 PNG" in result_text(result) else "The image failed: " + result_text(result))
        return s.finish("end_turn")

    if first.startswith("open app "):   # launches an app by path
        if step == 0:
            s.tool("toolu_app_1", "launch_app", {"path": first[len("open app "):].strip()})
            return s.finish("tool_use")
        s.text("The app is open.")
        return s.finish("end_turn")

    if "show the editor" in first:   # opens a sample file in the Text Editor
        if step == 0:
            s.tool("toolu_ed_1", "write_file", {"path": "notes/hello.rs", "content": SAMPLE_RS})
            return s.finish("tool_use")
        if step == 1:
            s.tool("toolu_ed_2", "launch_app", {"path": "notes/hello.rs"})
            return s.finish("tool_use")
        s.text("The editor is open.")
        return s.finish("end_turn")

    if first.startswith("open url "):   # diagnostic: a real site in Nova Browser
        if step == 0:
            s.tool("toolu_url_1", "open_url", {"url": first[len("open url "):].strip()})
            return s.finish("tool_use")
        s.text("The page is open.")
        return s.finish("end_turn")

    if "list my files" in first:
        if step == 0:
            s.tool("toolu_ls_1", "run_command", {"command": "ls"})
            return s.finish("tool_use")
        files = [l.split()[-1] for l in result_text(result).splitlines() if l.strip()]
        s.text("You have " + str(len(files)) + " files, including " + ", ".join(files[:3]) + ".")
        return s.finish("end_turn")

    if "open the test page" in first:
        if step == 0:
            s.tool("toolu_url_1", "open_url", {"url": f"http://10.0.2.2:{PORT}/test-page"})
            return s.finish("tool_use")
        s.text("The test page is open in Nova Browser.")
        return s.finish("end_turn")

    if "refuse" in first:
        s.text("I can")
        return s.finish("refusal", {"type": "refusal", "category": None, "explanation": "Mock refusal for testing."})

    s.text("Hello from the mock Claude. Ask me to build an app!")
    return s.finish("end_turn")


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        log("http: " + (fmt % args))

    def send_json(self, status, obj):
        data = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(data)

    # --- ComfyUI stand-in (Nova's generate_image tool) ---
    def comfy_png(self):
        w, h = 256, 144
        raw = b"".join(b"\0" + bytes([20, 120, 140]) * w for _ in range(h))
        def chunk(t, d):
            return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d))
        return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
                + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))

    def do_GET(self):
        if self.path.startswith("/history/"):
            pid = self.path.split("/")[2]
            done = pid in COMFY_JOBS
            return self.send_json(200, {pid: {"status": {"status_str": "success"}, "outputs": {"10": {"images": [
                {"filename": "nova_00001_.png", "subfolder": "", "type": "output"}]}}}} if done else {})
        if self.path.startswith("/view?"):
            data = self.comfy_png()
            self.send_response(200)
            self.send_header("Content-Type", "image/png")
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(data)
            return
        if self.path == "/models":
            ok = self.headers.get("Authorization") == f"Bearer {DEEPSEEK_KEY}"
            return self.send_json(200 if ok else 401, {"object": "list", "data": [{"id": "deepseek-flash", "object": "model"}]} if ok
                                  else {"error": {"message": "Authentication Fails", "type": "authentication_error"}})
        if self.path in ("/test.css", "/test-image.png"):
            log(f"browser fetched {self.path}")
            if self.path.endswith(".css"):
                # Large, like real sites' stylesheets (many rules), ending with the visible one.
                data = b"".join(b".r%d { margin: %dpx; }\n" % (i, i % 9) for i in range(20000)) + b"h1 { color: #3a6; font-size: 48px; }"
            else:
                art = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "assets", "home", "idea_game.png")
                data = open(art, "rb").read() if os.path.exists(art) else self.comfy_png()
            self.send_response(200)
            self.send_header("Content-Type", "text/css" if self.path.endswith(".css") else "image/png")
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(data)
            return
        if self.path.startswith("/test-page"):
            log(f"browser fetched /test-page, user agent: {self.headers.get('User-Agent', '')}")
            page = (b"<!doctype html><html><head><title>Nova test page</title>"
                    b"<script src='https://www.google-analytics.com/analytics.js'></script></head>"
                    b"<link rel='stylesheet' href='/test.css'>"
                    b"<body style='font-family:sans-serif'><h1>Hello from the test page</h1><img src='/test-image.png'></body></html>")
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", str(len(page)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(page)
            return
        if self.path.startswith("/v1/models"):
            if authorized(dict(self.headers.items())):
                self.send_json(200, {"data": [{"id": "claude-opus-5-5", "type": "model"}], "has_more": False})
            else:
                self.send_json(401, {"type": "error", "error": {"type": "authentication_error", "message": "invalid x-api-key"}})
            return
        self.send_json(404, {"type": "error", "error": {"type": "not_found_error", "message": "not found"}})

    def do_POST(self):
        if self.path == "/prompt":
            body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
            nodes = {n.get("class_type"): n.get("inputs", {}) for n in body.get("prompt", {}).values()}
            problems = []
            if nodes.get("UNETLoader", {}).get("unet_name") != "z_image_turbo_bf16.safetensors":
                problems.append("workflow should load z_image_turbo_bf16")
            lat = nodes.get("EmptySD3LatentImage", {})
            if (lat.get("width"), lat.get("height")) != (256, 144):
                problems.append(f"latent size {lat.get('width')}x{lat.get('height')}, wanted 256x144")
            if "SaveImage" not in nodes:
                problems.append("workflow has no SaveImage")
            for p in problems:
                log("VIOLATION: comfyui: " + p)
            pid = f"job{len(COMFY_JOBS) + 1}"
            COMFY_JOBS.add(pid)
            texts = [n["inputs"].get("text") for n in body.get("prompt", {}).values() if n.get("class_type") == "CLIPTextEncode"]
            log(f"comfyui prompt {pid}: {texts[0] if texts else ''!r}")
            return self.send_json(200, {"prompt_id": pid, "number": 1, "node_errors": {}})
        if self.path.startswith("/anthropic/"):   # DeepSeek's Messages API prefix
            self.path = self.path[len("/anthropic"):]
            if self.headers.get("x-api-key") != DEEPSEEK_KEY:
                log("VIOLATION: request under /anthropic without the DeepSeek key")
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length)
        try:
            body = json.loads(raw)
        except Exception as e:  # noqa: BLE001
            log(f"VIOLATION: request body is not JSON: {e}")
            return self.send_json(400, {"type": "error", "error": {"type": "invalid_request_error", "message": "body is not JSON"}})
        headers = {k.lower(): v for k, v in self.headers.items()}
        if not authorized(headers):
            return self.send_json(401, {"type": "error", "error": {"type": "authentication_error", "message": "invalid x-api-key"}})
        problems = validate(headers, body)
        first = first_user_text(body.get("messages", [{}]))
        log(f"request: {len(body.get('messages', []))} messages, first={first[:60]!r}, problems={problems}")
        if problems:
            for p in problems:
                log("VIOLATION: " + p)
            return self.send_json(400, {"type": "error", "error": {"type": "invalid_request_error", "message": "; ".join(problems)}})
        if "overloaded" in first:
            n = ATTEMPTS.get(first, 0)
            ATTEMPTS[first] = n + 1
            if n == 0:
                return self.send_json(529, {"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}})
        events = respond(body)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Transfer-Encoding", "chunked")
        self.send_header("Connection", "close")
        self.end_headers()
        for e in events:
            self.wfile.write(f"{len(e):x}\r\n".encode() + e + b"\r\n")
            self.wfile.flush()
        self.wfile.write(b"0\r\n\r\n")


PORT = 0


def main():
    global LOG, PORT
    port = int(sys.argv[1])
    PORT = port
    LOG = open(sys.argv[2], "a", buffering=1)
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    log(f"mock Anthropic API listening on 127.0.0.1:{port}")
    server.serve_forever()


if __name__ == "__main__":
    main()
