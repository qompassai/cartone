#!/usr/bin/env python3
"""Cartone rung-2 smoke driver: real end-to-end jobs through the MCP.

Spawns the cartone binary over stdio (modern era: server/discover
handshake, protocol version carried in params._meta, exactly the
wire shape of the repo's tests/stdio.rs) and runs one real job per
local route:

  ltx-video -> artifacts/<job_id>.mp4   (ftyp magic + size verified;
               runs first, while the GPU is empty — its input is
               the standing smoke/ltx-input.png, seeded from the
               wai route's own verified output of the first rung-2
               smoke run, job-000001)
  wai       -> artifacts/<job_id>.png   (PNG magic + size verified)
  kokoro    -> artifacts/<job_id>/take0.wav (RIFF magic + size)

Then prints ledger_summary and a final JSON verdict line. Exit 0
only if every phase completed and verified. The server is spawned
fresh for the smoke and exits when stdin closes; the jobs it ran
stay in the data dir's ledger.jsonl as the gate record.
"""

import argparse
import json
import os
import subprocess
import sys
import threading
import time

META = ('"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28",'
        '"io.modelcontextprotocol/clientInfo":{"name":"smoke","version":"0"},'
        '"io.modelcontextprotocol/clientCapabilities":{}}')

TERMINAL = ("completed", "failed", "cancelled", "denied")


class Session:
    def __init__(self, binary, data_dir, routes_file):
        env = dict(os.environ)
        env["CARTONE_DATA_DIR"] = data_dir
        env["CARTONE_ROUTES_FILE"] = routes_file
        self.child = subprocess.Popen(
            [binary], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, env=env, text=True)
        self.lines = []
        self.cond = threading.Condition()

        def reader():
            for line in self.child.stdout:
                with self.cond:
                    self.lines.append(line)
                    self.cond.notify_all()

        threading.Thread(target=reader, daemon=True).start()
        self.next_id = 0

    def call(self, method, params_inner=""):
        self.next_id += 1
        rid = self.next_id
        if params_inner:
            params = "{%s,%s}" % (params_inner, META)
        else:
            params = "{%s}" % META
        frame = '{"jsonrpc":"2.0","id":%d,"method":"%s","params":%s}' % (
            rid, method, params)
        self.child.stdin.write(frame + "\n")
        self.child.stdin.flush()
        deadline = time.time() + 60
        with self.cond:
            while time.time() < deadline:
                for line in self.lines:
                    value = json.loads(line)
                    if value.get("id") == rid:
                        return value
                self.cond.wait(timeout=1)
        raise TimeoutError("no response for id %d" % rid)

    def tool(self, name, arguments):
        inner = '"name":"%s","arguments":%s' % (name, json.dumps(arguments))
        resp = self.call("tools/call", inner)
        text = resp["result"]["content"][0]["text"]
        return json.loads(text)

    def close(self):
        self.child.stdin.close()
        self.child.wait(timeout=30)


def poll_job(sess, job_id, timeout_s):
    deadline = time.time() + timeout_s
    record = None
    while time.time() < deadline:
        record = sess.tool("get_job", {"job_id": job_id})
        if record["status"] in TERMINAL:
            return record
        time.sleep(5)
    return record


def check_file(path, magic, label):
    if not os.path.isfile(path):
        return "%s missing: %s" % (label, path)
    size = os.path.getsize(path)
    with open(path, "rb") as fh:
        head = fh.read(len(magic))
    if head != magic:
        return "%s bad magic at %s" % (label, path)
    if size < 1024:
        return "%s suspiciously small: %d bytes" % (label, size)
    print("%s verified: %s (%d bytes)" % (label, path, size))
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True)
    ap.add_argument("--data-dir", required=True)
    ap.add_argument("--routes", required=True)
    args = ap.parse_args()

    artifacts = os.path.join(args.data_dir, "artifacts")
    os.makedirs(artifacts, exist_ok=True)
    sess = Session(args.bin, args.data_dir, args.routes)
    verdict = {}
    try:
        info = sess.call("server/discover")
        print("discover:", json.dumps(info["result"])[:400])
        routes = sess.tool("list_routes", {})
        for route in routes["routes"]:
            print("route %-10s configured=%s available=%s"
                  % (route["name"], route["configured"], route["available"]))

        # --- ltx-video (standing input, GPU still empty) ---------
        ltx_input = os.path.join(args.data_dir, "smoke", "ltx-input.png")
        if not os.path.isfile(ltx_input):
            verdict["ltx-video"] = "standing input missing: %s" % ltx_input
        else:
            job = sess.tool("submit_job", {"route": "ltx-video",
                                           "lane": "sfw", "kind": "video",
                                           "units": 1})
            print("ltx submitted:", job["job_id"], job["status"])
            rec = poll_job(sess, job["job_id"], 2100)
            print("ltx final:", json.dumps(rec))
            if rec["status"] != "completed":
                verdict["ltx-video"] = "ltx job status %s" % rec["status"]
            else:
                path = os.path.join(artifacts, rec["job_id"] + ".mp4")
                problem = None
                if not os.path.isfile(path):
                    problem = "mp4 missing: %s" % path
                else:
                    with open(path, "rb") as fh:
                        head = fh.read(12)
                    if head[4:8] != b"ftyp":
                        problem = "mp4 bad magic"
                    elif os.path.getsize(path) < 10240:
                        problem = "mp4 suspiciously small"
                    else:
                        print("ltx mp4 verified: %s (%d bytes)"
                              % (path, os.path.getsize(path)))
                verdict["ltx-video"] = problem or "ok"

        # --- wai -------------------------------------------------
        job = sess.tool("submit_job", {"route": "wai", "lane": "sfw",
                                       "kind": "image", "units": 1})
        print("wai submitted:", job["job_id"], job["status"])
        rec = poll_job(sess, job["job_id"], 1500)
        print("wai final:", json.dumps(rec))
        err = None
        if rec["status"] != "completed":
            err = "wai job status %s" % rec["status"]
        else:
            err = check_file(os.path.join(artifacts, rec["job_id"] + ".png"),
                             b"\x89PNG\r\n\x1a\n", "wai png")
        verdict["wai"] = err or "ok"

        # --- kokoro ----------------------------------------------
        job = sess.tool("submit_job", {"route": "kokoro", "lane": "sfw",
                                       "kind": "tts", "units": 1})
        print("kokoro submitted:", job["job_id"], job["status"])
        rec = poll_job(sess, job["job_id"], 1500)
        print("kokoro final:", json.dumps(rec))
        if rec["status"] != "completed":
            verdict["kokoro"] = "kokoro job status %s" % rec["status"]
        else:
            err = check_file(
                os.path.join(artifacts, rec["job_id"], "take0.wav"),
                b"RIFF", "kokoro wav")
            verdict["kokoro"] = err or "ok"

        summary = sess.tool("ledger_summary", {})
        print("ledger_summary:", json.dumps(summary))
        verdict["ledger_summary"] = summary
    finally:
        sess.close()
    print("VERDICT " + json.dumps(verdict))
    ok = all(verdict[k] == "ok" for k in ("wai", "ltx-video", "kokoro"))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
