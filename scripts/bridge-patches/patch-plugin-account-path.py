#!/usr/bin/env python3
"""修正桥插件传给 AV Host 的 accountPath。

NapCat 4.18.x 不再提供 session.getAccountPath，插件于是回退到 ctx.core.dataPath，
拿到的是 QQ 数据根目录（/app/.config/QQ）。但 AVSDK 登录期望的是该账号自己的
nt_qq_<hash> 目录；路径不一致时 AVSDK 会立刻被 QQ 服务器踢下线（输出命令 20050），
插件随即无限重登（实测累计 522 次），来电也就永远等不到接听回调。

这里只做一处最小、幂等的替换：在 dataPath 下找 nt_qq_* 目录作为 accountPath，
并在路径变化时打一行日志，方便日后确认。

幂等：已打过补丁直接返回。可逆：首次修改前备份为 index.mjs.upstream。
"""
import pathlib
import sys

PLUGIN = pathlib.Path(
    "/app/napcat/plugins/napcat-plugin-maibot-qq-voice-call/index.mjs"
)
BACKUP = PLUGIN.with_name("index.mjs.upstream")

OLD = """      const accountPath = String(
        session?.getAccountPath?.(Number.parseInt(selfUin, 10)) || ctx.core?.dataPath || "",
      );"""

NEW = """      const accountPath = String(
        session?.getAccountPath?.(Number.parseInt(selfUin, 10)) ||
          resolveAccountPath(ctx.core?.dataPath, selfUin) ||
          ctx.core?.dataPath ||
          "",
      );
      if (resolveAccountPath.__lastPath !== accountPath) {
        resolveAccountPath.__lastPath = accountPath;
        logger?.info(`[MaiBotQQCall] AV Host 登录参数 accountPath=${accountPath}`);
      }"""

HELPER = '''
/**
 * NapCat 4.18.x 起 session.getAccountPath 不存在，只用 ctx.core.dataPath 会拿到
 * QQ 数据根目录，而 AVSDK 期望的是该账号自己的 nt_qq_<hash> 目录。
 */
function resolveAccountPath(dataPath, uin) {
  try {
    if (!dataPath) return "";
    const entries = fs.readdirSync(dataPath, { withFileTypes: true });
    const dirs = entries
      .filter((entry) => entry.isDirectory() && entry.name.startsWith("nt_qq_"))
      .map((entry) => path.join(dataPath, entry.name));
    return dirs[0] || "";
  } catch {
    return "";
  }
}
'''


def main() -> int:
    if not PLUGIN.is_file():
        print(f"[patch] 找不到插件 {PLUGIN}", file=sys.stderr)
        return 1
    src = PLUGIN.read_text(encoding="utf-8")
    if "resolveAccountPath" in src:
        print("[patch] accountPath 补丁已在位")
        return 0
    if OLD not in src:
        print("[patch] 未找到 accountPath 锚点（插件版本可能变了）", file=sys.stderr)
        return 2
    if not BACKUP.exists():
        BACKUP.write_text(src, encoding="utf-8")
        print(f"[patch] 已备份上游插件到 {BACKUP.name}")
    src = src.replace(OLD, NEW, 1)
    src = src.replace("function scheduleAVHostLogin(", HELPER + "\nfunction scheduleAVHostLogin(", 1)
    PLUGIN.write_text(src, encoding="utf-8")
    print("[patch] 已修正 accountPath 解析")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
