#!/usr/bin/env python3
"""桌面资源暂存与 bundle 组装；不调用 shell、不覆盖已有文件。"""
from __future__ import annotations

import argparse
import os
import plistlib
import re
import shutil
import struct
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SIZES = (16, 24, 32, 48, 64, 128, 256, 512, 1024)
# 严格 SemVer（Major.Minor.Patch + 可选预发布/构建段）；发版链的版本号都先过这里
SEMVER_RE = re.compile(
    r"(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?"
)
# deb 随包文档（Debian 惯例放 /usr/share/doc/<包名>/）
DEB_DOC_FILES = ("README.md", "CHANGELOG.md", "LICENSE")


def no_symlink(path: Path) -> None:
    for item in (path, *path.parents):
        if item.is_symlink():
            raise ValueError(f"不接受符号链接目标: {item}")


def absolute_path(value: str) -> Path:
    path = Path(value)
    if not path.is_absolute() or any(char in value for char in "\r\n\0"):
        raise ValueError("路径必须为绝对路径，且不能包含控制字符")
    no_symlink(path)
    return path


def binary_path(value: str | None) -> Path:
    candidate = value or shutil.which("laterterm")
    if candidate is None:
        raise ValueError("未找到 laterterm，请指定 --binary")
    path = absolute_path(candidate)
    if not path.is_file() or not os.access(path, os.X_OK):
        raise ValueError("--binary 必须是可执行的普通文件")
    return path


def png_bytes(path: Path, size: int) -> bytes:
    data = path.read_bytes()
    if (len(data) < 24 or data[:8] != b"\x89PNG\r\n\x1a\n"
            or data[12:16] != b"IHDR" or struct.unpack(">II", data[16:24]) != (size, size)):
        raise ValueError(f"PNG 资源无效或尺寸错误: {path}")
    return data


def icns_bytes(path: Path) -> bytes:
    data = path.read_bytes()
    if len(data) < 8 or data[:4] != b"icns" or int.from_bytes(data[4:8], "big") != len(data):
        raise ValueError(f"ICNS 资源无效: {path}")
    return data


def desktop_exec(path: str) -> str:
    # Desktop Entry 的字符串先解码转义，再解析 Exec；% 必须双写，不能误作字段码。
    quoted = path.replace("%", "%%").replace("\\", "\\\\")
    for char in ('"', '`', '$'):
        quoted = quoted.replace(char, "\\" + char)
    return '"' + quoted.replace("\\", "\\\\") + '"'


def write_new_files(files: dict[Path, tuple[bytes, int]]) -> None:
    # 先校验完整集合，缺图或已有目标时不创建部分安装。
    for path in files:
        no_symlink(path)
        for parent in path.parents:
            if parent.exists() and not parent.is_dir():
                raise ValueError(f"目标父路径不是目录: {parent}")
        if path.exists():
            raise ValueError(f"拒绝覆盖已有目标: {path}")
    for path, (data, mode) in files.items():
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("xb") as output:
            output.write(data)
        path.chmod(mode)


def workspace_version() -> str:
    # 版本读取不依赖字段行序：grep '^version' 在 workspace 字段重排时会静默拿错值
    version = str(tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"])
    if not SEMVER_RE.fullmatch(version):
        raise ValueError(f"Cargo.toml 版本号不是合法 SemVer: {version}")
    return version


def render_desktop(launch_path: str) -> bytes:
    # GIO 对 executable 内的 % 即使双写也不可靠；拒绝而非安装不可启动的入口。
    if "%" in launch_path:
        raise ValueError("可执行文件安装路径不能包含百分号，请使用不含 % 的用户目录或 --destdir 暂存")
    template = (ROOT / "packaging/linux/laterterm.desktop").read_text(encoding="utf-8")
    desktop = re.sub(r"(?m)^Exec=.*$", lambda _: "Exec=" + desktop_exec(launch_path), template)
    return desktop.encode("utf-8")


def hicolor_icons() -> dict[Path, tuple[bytes, int]]:
    """相对 usr/share 的 hicolor 图标集合（Linux 暂存与 deb 共用，不另复制源图）。"""
    files: dict[Path, tuple[bytes, int]] = {}
    for size in SIZES:
        source = ROOT / f"assets/icons/png/laterterm-{size}.png"
        files[Path(f"icons/hicolor/{size}x{size}/apps/laterterm.png")] = (png_bytes(source, size), 0o644)
    return files


def install_linux(args: argparse.Namespace) -> None:
    binary = binary_path(args.binary)
    if args.destdir is not None:
        dest = absolute_path(args.destdir)
        if dest.resolve() == Path("/"):
            raise ValueError("--destdir 不能为根目录")
        bindir = dest / "usr/bin"
        datadir = dest / "usr/share"
        launch_path = "/usr/bin/laterterm"
    else:
        home_value = os.environ.get("HOME")
        if not home_value:
            raise ValueError("缺少 HOME，无法确定用户安装位置")
        home = absolute_path(home_value)
        bindir = home / ".local/bin"
        data_value = os.environ.get("XDG_DATA_HOME")
        datadir = absolute_path(data_value) if data_value and Path(data_value).is_absolute() else home / ".local/share"
        launch_path = str(bindir / "laterterm")
    files = {
        bindir / "laterterm": (binary.read_bytes(), 0o755),
        datadir / "applications/laterterm.desktop": (render_desktop(launch_path), 0o644),
    }
    files.update({datadir / relative: entry for relative, entry in hicolor_icons().items()})
    write_new_files(files)
    print(f"桌面资源已安装: {datadir}；可执行文件: {bindir / 'laterterm'}")


def package_macos(args: argparse.Namespace) -> None:
    binary = binary_path(args.binary)
    output = absolute_path(args.output)
    if output.resolve() == Path("/"):
        raise ValueError("--output 不能为根目录")
    app = output / "LaterTerm.app"
    no_symlink(app)
    if app.exists():
        raise ValueError(f"拒绝覆盖已有 bundle: {app}")
    version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
    template = (ROOT / "packaging/macos/Info.plist.in").read_bytes()
    metadata = plistlib.loads(template.replace(b"@VERSION@", version.encode("utf-8")))
    files = {
        app / "Contents/MacOS/laterterm": (binary.read_bytes(), 0o755),
        app / "Contents/Info.plist": (plistlib.dumps(metadata, sort_keys=False), 0o644),
        app / "Contents/Resources/laterterm.icns": (icns_bytes(ROOT / "assets/icons/laterterm.icns"), 0o644),
    }
    write_new_files(files)
    print(f"已组装未签名 bundle: {app}（未验证二进制架构或动态库可分发性）")


def package_deb(args: argparse.Namespace) -> None:
    binary = binary_path(args.binary)
    output = absolute_path(args.output)
    if output.resolve() == Path("/"):
        raise ValueError("--output 不能为根目录")
    no_symlink(output)
    if output.exists():
        raise ValueError(f"拒绝覆盖已有暂存目录: {output}")
    version = workspace_version()
    deb = absolute_path(args.deb_path) if args.deb_path else output.parent / f"laterterm_{version}_amd64.deb"
    if deb == output or output in deb.parents:
        raise ValueError(".deb 输出不能位于暂存目录内部")
    if deb.exists():
        raise ValueError(f"拒绝覆盖已有 .deb: {deb}")
    control = "\n".join([
        "Package: laterterm",
        f"Version: {version}",
        "Section: utils",
        "Priority: optional",
        "Architecture: amd64",
        "Maintainer: crazykun <jilikun@gmail.com>",
        "Homepage: https://github.com/crazykun/LaterTerm",
        # 依赖首版只写宽松下限（实机 apt install 验收后再收紧，不写 Recommends）；
        # libasound2 在 Ubuntu 24.04 因 time64 迁移改名 libasound2t64，用 alternatives 兼容。
        "Depends: libc6 (>= 2.31), libasound2 | libasound2t64, libfontconfig1, libudev1, "
        "libwayland-client0, libx11-6, libxcb1, libxkbcommon-x11-0, libxkbcommon0 (>= 0.5)",
        "Description: LaterTerm - GPU 加速终端",
        " 跨平台 GPU 终端：Deepin 终端功能 × Tabby 颜值 × 原生性能。",
        "",
    ]).encode("utf-8")
    files = {
        output / "usr/bin/laterterm": (binary.read_bytes(), 0o755),
        output / "usr/share/applications/laterterm.desktop": (render_desktop("/usr/bin/laterterm"), 0o644),
        output / "DEBIAN/control": (control, 0o644),
    }
    for name in DEB_DOC_FILES:
        source = ROOT / name
        if not source.is_file():
            raise ValueError(f"缺少随包文档: {name}")
        files[output / f"usr/share/doc/laterterm/{name}"] = (source.read_bytes(), 0o644)
    files.update({output / "usr/share" / relative: entry for relative, entry in hicolor_icons().items()})
    write_new_files(files)

    if args.skip_dpkg_deb:
        print(f"已暂存 deb 目录树: {output}（--skip-dpkg-deb，未组包）")
        return
    dpkg = shutil.which("dpkg-deb")
    if dpkg is None:
        raise ValueError("未找到 dpkg-deb；无 dpkg 的环境请加 --skip-dpkg-deb 仅产暂存树")
    result = subprocess.run([dpkg, "--build", "--root-owner-group", str(output), str(deb)], text=True, capture_output=True)
    if result.returncode != 0:
        raise ValueError(f"dpkg-deb 组包失败: {(result.stderr or result.stdout).strip()}")
    # 组包后立即 --info 自校验，control 畸形当场红而不是装包时才发现
    info = subprocess.run([dpkg, "--info", str(deb)], text=True, capture_output=True)
    if info.returncode != 0:
        raise ValueError(f"dpkg-deb --info 自校验失败: {(info.stderr or info.stdout).strip()}")
    print(f"已组包: {deb}\n{info.stdout.strip()}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_subparsers(dest="mode", required=True)
    linux = modes.add_parser("linux", help="用户级安装或显式暂存到 --destdir")
    linux.add_argument("--binary")
    linux.add_argument("--destdir")
    macos = modes.add_parser("macos", help="从已构建的二进制组装未签名 .app")
    macos.add_argument("--binary", required=True)
    macos.add_argument("--output", required=True)
    deb = modes.add_parser("deb", help="staging deb 目录树并用 dpkg-deb 组包")
    deb.add_argument("--binary")
    deb.add_argument("--output", required=True)
    deb.add_argument("--deb-path")
    deb.add_argument("--skip-dpkg-deb", action="store_true")
    args = parser.parse_args()
    handlers = {"linux": install_linux, "macos": package_macos, "deb": package_deb}
    try:
        handlers[args.mode](args)
    except (OSError, ValueError, KeyError) as error:
        print(f"错误: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
