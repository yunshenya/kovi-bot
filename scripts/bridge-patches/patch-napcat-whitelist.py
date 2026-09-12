#!/usr/bin/env python3
"""把 QQ 通话桥插件加入 NapCat 内置的官方插件白名单。

NapCat 4.18.x 在 napcat.mjs 里硬编码了一份只含 4 个官方插件的白名单
（见 isOfficialPlugin / getRejectReason），第三方插件一律被
"not in official plugin whitelist" 拒绝。这里只做一处最小、可逆、幂等的追加。

要覆盖两个位置，因为容器首次启动时 napcat.mjs 还不存在（由 entrypoint.sh
从 NapCat.Shell.zip 解压得到），只有当入口包装脚本在它之前运行时：

  1. /app/NapCat.Shell.zip 里的 napcat.mjs —— 模板；改它，解压出来就是打过补丁的；
  2. /app/napcat/napcat.mjs     —— 已经解压出来的运行时文件。

幂等：两处都已包含插件名就直接返回。可逆：首次修改前各留一份 *.kovi-orig。
"""
import pathlib
import re
import shutil
import sys
import zipfile

PLUGIN = "napcat-plugin-maibot-qq-voice-call"
ANCHOR = re.compile(r'(new Set\(\[\s*"napcat-plugin-builtin",)')
RUNTIME = pathlib.Path("/app/napcat/napcat.mjs")
TEMPLATE = pathlib.Path("/app/NapCat.Shell.zip")
ZIP_MEMBER = "napcat.mjs"


def patch_source(src: str) -> str | None:
    """返回打过补丁的源码；已打过或锚点不匹配时返回 None。"""
    if f'"{PLUGIN}"' in src:
        return None
    patched, count = ANCHOR.subn(lambda m: f'{m.group(1)}\n  "{PLUGIN}",', src, count=1)
    return patched if count == 1 else None


def patch_runtime() -> str:
    if not RUNTIME.is_file():
        return "运行时 napcat.mjs 尚未解压，跳过"
    src = RUNTIME.read_text(encoding="utf-8", errors="replace")
    patched = patch_source(src)
    if patched is None:
        return "运行时白名单已包含桥插件" if f'"{PLUGIN}"' in src else "运行时锚点未匹配（NapCat 版本可能已变）"
    backup = RUNTIME.with_name(RUNTIME.name + ".kovi-orig")
    if not backup.exists():
        shutil.copy2(RUNTIME, backup)
    RUNTIME.write_text(patched, encoding="utf-8")
    return "已给运行时 napcat.mjs 打补丁"


def patch_template() -> str:
    if not TEMPLATE.is_file():
        return "模板 zip 不存在，跳过"
    with zipfile.ZipFile(TEMPLATE) as archive:
        if ZIP_MEMBER not in archive.namelist():
            return "模板 zip 里没有 napcat.mjs，跳过"
        src = archive.read(ZIP_MEMBER).decode("utf-8", "replace")
        patched = patch_source(src)
        if patched is None:
            return "模板白名单已包含桥插件" if f'"{PLUGIN}"' in src else "模板锚点未匹配（NapCat 版本可能已变）"
        items = [(info, archive.read(info.filename)) for info in archive.infolist()]
    backup = TEMPLATE.with_name(TEMPLATE.name + ".kovi-orig")
    if not backup.exists():
        shutil.copy2(TEMPLATE, backup)
    temporary = TEMPLATE.with_name(TEMPLATE.name + ".kovi-tmp")
    with zipfile.ZipFile(temporary, "w", zipfile.ZIP_DEFLATED) as out:
        for info, payload in items:
            if info.filename == ZIP_MEMBER:
                payload = patched.encode("utf-8")
            out.writestr(info, payload)
    temporary.replace(TEMPLATE)
    return "已给模板 zip 打补丁"


def main() -> int:
    if not RUNTIME.is_file() and not TEMPLATE.is_file():
        print("[patch] 运行时与模板都不存在，无法打补丁", file=sys.stderr)
        return 1
    for step in (patch_runtime, patch_template):
        message = step()
        print(f"[patch] {message}")
        if "锚点未匹配" in message:
            return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
