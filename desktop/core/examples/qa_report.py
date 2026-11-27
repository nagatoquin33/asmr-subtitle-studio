#!/usr/bin/env python3
"""全片测试结果验收：段数/翻译覆盖/幻觉过滤/时间轴完整性/抽样展示。"""
import json, io, sys, os

sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding='utf-8', errors='replace')
home = sys.argv[1]  # e2e home 目录
tasks_dir = os.path.join(home, 'data', 'tasks')
t = sorted(os.listdir(tasks_dir))[-1]
tdir = os.path.join(tasks_dir, t)
task = json.load(open(os.path.join(tdir, 'task.json'), encoding='utf-8'))
segs = json.load(open(os.path.join(tdir, 'segments.json'), encoding='utf-8'))

print(f"任务: {t}")
print(f"状态: {task['status']} | {task['message']}")
print(f"媒体: {task['media'].get('duration', 0):.0f}s, {task['media'].get('size', 0)/1048576:.0f}MB")
print(f"ASR:  {json.dumps(task.get('asr', {}), ensure_ascii=False)}")
n = len(segs)
translated = sum(1 for s in segs if s.get('translated', '').strip() and s['translated'] != s['text'])
same_as_src = sum(1 for s in segs if s.get('translated', '').strip() == s['text'])
flagged = sum(1 for s in segs if s.get('flagged'))
empty = sum(1 for s in segs if not s.get('translated', '').strip())
print(f"\n段数: {n} | 已翻译: {translated} | 译文=原文: {same_as_src} | 空译文: {empty} | 幻觉标记: {flagged}")
# 时间轴完整性：覆盖时长
if segs:
    dur = sum(s['end'] - s['start'] for s in segs)
    print(f"字幕总时长: {dur:.0f}s / 视频_duration {task['media'].get('duration', 0):.0f}s ({dur/max(task['media'].get('duration',1),1)*100:.0f}%)")

# 连续重复游程（过滤后应大幅减少）
runs, cur, cnt = [], segs[0]['text'], 1
for s in segs[1:]:
    if s['text'] == cur: cnt += 1
    else:
        if cnt >= 3: runs.append((cur[:25], cnt))
        cur, cnt = s['text'], 1
if cnt >= 3: runs.append((cur[:25], cnt))
print(f"过滤后连续重复>=3 游程: {len(runs)} 个")
for txt, c in runs[:6]: print(f"   x{c}: {txt}")

print("\n--- 开头 5 段 ---")
for s in segs[:5]:
    print(f"[{s['start']:7.1f}-{s['end']:7.1f}] {s['text'][:50]}")
    print(f"{'':>18}→ {s.get('translated','')[:50]}")
print("\n--- 中段 5 段 ---")
for s in segs[n//2:n//2+5]:
    print(f"[{s['start']:7.1f}-{s['end']:7.1f}] {s['text'][:50]}")
    print(f"{'':>18}→ {s.get('translated','')[:50]}")
print("\n--- 结尾 5 段 ---")
for s in segs[-5:]:
    print(f"[{s['start']:7.1f}-{s['end']:7.1f}] {s['text'][:50]}")
    print(f"{'':>18}→ {s.get('translated','')[:50]}")
