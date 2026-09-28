"""端到端测试：对运行中的后端 (127.0.0.1:8765) 提交样例音频，验证 全流程。

用法：
    python scripts/e2e_test.py                # 英文 wav → 中文
    python scripts/e2e_test.py --media data/samples/sample_zh.mp4 --target en
"""
from __future__ import annotations

import argparse
import sys
import time
from pathlib import Path

import httpx

BASE = "http://127.0.0.1:8765"


def wait_task(client: httpx.Client, tid: str, timeout: float = 600) -> dict:
    t0 = time.time()
    last = ""
    while time.time() - t0 < timeout:
        t = client.get(f"/api/tasks/{tid}").json()
        line = f"[{t['status']}] {t['progress']*100:.0f}% {t['message']}"
        if line != last:
            print("  ", line)
            last = line
        if t["status"] in ("done", "error", "canceled"):
            return t
        time.sleep(1.5)
    raise TimeoutError("任务超时")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--media", default="data/samples/sample_en.wav")
    ap.add_argument("--source", default="auto")
    ap.add_argument("--target", default="zh")
    args = ap.parse_args()

    media = Path(args.media)
    assert media.exists(), f"样例不存在: {media}"

    with httpx.Client(base_url=BASE, timeout=60) as c:
        h = c.get("/api/health").json()
        print("health:", h["ok"], "ffmpeg:", h["ffmpeg"], "gpu:", h["gpu"]["available"], h["gpu"]["name"])
        assert h["ffmpeg"], "ffmpeg 不可用"

        # 开启 mock 翻译，保证无 API Key 也能跑通
        c.post("/api/config", json={"translate": {"mock": True}}).raise_for_status()

        r = c.post(
            "/api/tasks",
            files={"files": (media.name, open(media, "rb"), "application/octet-stream")},
            data={"source_lang": args.source, "target_lang": args.target},
        )
        r.raise_for_status()
        task = r.json()["tasks"][0]
        tid = task["id"]
        print("task:", tid, task["name"])

        t = wait_task(c, tid)
        if t["status"] != "done":
            print("FAILED:", t.get("error"))
            return 1

        detail = c.get(f"/api/tasks/{tid}", params={"segments": 1}).json()
        segs = detail["segments"]
        print(f"\n=== {len(segs)} 段 ===")
        for s in segs[:8]:
            print(f"[{s['start']:7.2f}-{s['end']:7.2f}] {s['text']}  =>  {s['translated']}")

        for ftype in ("orig_srt", "trans_srt", "bilingual_srt", "bilingual_ass", "bilingual_vtt"):
            r = c.get(f"/api/tasks/{tid}/download", params={"type": ftype})
            ok = r.status_code == 200 and len(r.content) > 0
            print(f"download {ftype}: {'OK' if ok else 'FAIL(' + str(r.status_code) + ')'}")
            if ftype == "bilingual_srt" and ok:
                print("--- bilingual.srt 预览 ---")
                print(r.content.decode("utf-8-sig")[:400])

        # 单句编辑
        if segs:
            new = segs[0]["translated"] + "（已编辑）"
            r = c.patch(f"/api/tasks/{tid}/segments/0", json={"translated": new})
            print("patch segment:", "OK" if r.status_code == 200 and r.json()["segment"]["translated"] == new else "FAIL")

        # 清理：删除任务并关闭 mock
        c.delete(f"/api/tasks/{tid}").raise_for_status()
        c.post("/api/config", json={"translate": {"mock": False}}).raise_for_status()
        print("\nE2E PASS ✅")
        return 0


if __name__ == "__main__":
    sys.exit(main())
