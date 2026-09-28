"""启动入口：开发用 `python run.py`，打包后双击 exe。

行为：
  - 优先以原生桌面窗口运行（pywebview / Windows WebView2），不可用时回退浏览器
  - 端口上已有**本程序**实例时，不再另起服务，而是为它打开一个桌面窗口（不再弹浏览器）
  - 端口被其他程序占用时自动换下一个端口
参数：
  --browser     强制浏览器模式
  --no-browser  纯服务模式，不打开任何窗口（ASMR_NO_BROWSER=1 等价）
  --port N      指定起始端口（默认 8765）
"""
from __future__ import annotations

import json
import os
import socket
import sys
import threading
import time
import urllib.request
import webbrowser

PORT = 8765


def _port_busy(port: int) -> bool:
    with socket.socket() as s:
        s.settimeout(1)
        return s.connect_ex(("127.0.0.1", port)) == 0


def _is_our_app(port: int) -> bool:
    """端口上的服务是否为本程序实例（据此决定附着还是换端口）。"""
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/api/health", timeout=2) as r:
            return json.loads(r.read().decode()).get("app") == "asmr-subtitle-studio"
    except Exception:
        return False


def _pick_port() -> tuple[int, bool]:
    """返回 (port, attach)。attach=True 表示该端口已有本程序实例。"""
    if not _port_busy(PORT):
        return PORT, False
    if _is_our_app(PORT):
        return PORT, True
    for p in range(PORT + 1, PORT + 20):
        if not _port_busy(p):
            return p, False
    return PORT, False


def _open_browser(url: str, delay: float = 0.8) -> None:
    def _open():
        time.sleep(delay)
        try:
            webbrowser.open(url)
        except Exception:
            pass

    threading.Thread(target=_open, daemon=True).start()


def _serve_background(port: int) -> None:
    import uvicorn

    from backend.app import app

    conf = uvicorn.Config(app, host="127.0.0.1", port=port, log_level="warning")
    server = uvicorn.Server(conf)
    threading.Thread(target=server.run, daemon=True).start()
    for _ in range(80):  # 最多等 20s 直到端口就绪
        if _port_busy(port):
            return
        time.sleep(0.25)


def _window(url: str) -> None:
    import webview

    try:
        webview.settings["ALLOW_DOWNLOADS"] = True  # 允许在窗口内下载字幕文件
    except Exception:
        pass
    webview.create_window(
        "ASMR 字幕工作室",
        url,
        width=1440,
        height=920,
        min_size=(1080, 700),
        background_color="#0d0f14",
    )
    webview.start()  # 阻塞到窗口关闭，随后进程退出


def _run_in_window(port: int) -> None:
    url = f"http://127.0.0.1:{port}"
    _serve_background(port)
    print(f"[启动] ASMR 字幕工作室（桌面窗口，关闭窗口即退出）{url}")
    _window(url)


def _run_in_console(open_page: bool, port: int = PORT) -> None:
    import uvicorn

    from backend.app import app

    if open_page:
        _open_browser(f"http://127.0.0.1:{port}")
    print(f"[启动] ASMR 字幕工作室  http://127.0.0.1:{port}  （关闭此窗口即退出）")
    uvicorn.run(app, host="127.0.0.1", port=port, log_level="warning")


def _pause(msg: str) -> None:
    print(msg)
    try:
        input()
    except EOFError:
        pass


def _main() -> None:
    argv = sys.argv[1:]
    no_browser = "--no-browser" in argv or os.environ.get("ASMR_NO_BROWSER") == "1"
    force_browser = "--browser" in argv or os.environ.get("ASMR_BROWSER") == "1"

    port = PORT
    if "--port" in argv:
        i = argv.index("--port")
        if i + 1 < len(argv):
            try:
                port = int(argv[i + 1])
            except ValueError:
                pass

    port, attach = _pick_port()
    url = f"http://127.0.0.1:{port}"

    if attach:
        print(f"[提示] 检测到本程序已在 {url} 运行——直接为它打开桌面窗口")
        if not no_browser and not force_browser:
            try:
                _window(url)
                return
            except Exception as e:
                print(f"[提示] 桌面窗口不可用（{str(e)[:120]}），改用浏览器")
        webbrowser.open(url)
        _pause("按回车键退出…")
        return

    if no_browser:
        _run_in_console(open_page=False, port=port)
        return

    if not force_browser:
        try:
            _run_in_window(port)
            return
        except Exception as e:
            print(f"[提示] 桌面窗口启动失败（{str(e)[:160]}），回退浏览器模式")
    _run_in_console(open_page=True, port=port)


def main() -> None:
    try:
        _main()
    except KeyboardInterrupt:
        pass
    except SystemExit:
        raise
    except Exception:
        # 任何启动错误都让控制台停住显示，避免窗口一闪而过看不到原因
        import traceback

        traceback.print_exc()
        _pause("启动失败，按回车键退出…")
        raise SystemExit(1)


if __name__ == "__main__":
    main()
