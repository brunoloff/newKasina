#!/usr/bin/env python3
"""Create native desktop downloads, then optionally smoke-test the packaged app.

Requires Python 3.11+. macOS packaging uses the standard Xcode command-line tools.
No external Python modules, signing credentials, or packaging frameworks are needed.
"""

import argparse
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import shutil
import struct
import subprocess
import tempfile
import tomllib


ROOT = Path(__file__).resolve().parent.parent
MAC_RESOURCES = ROOT / "packaging" / "macos"


def run(*args, **kwargs):
    return subprocess.run([str(arg) for arg in args], check=True, **kwargs)


def output(*args):
    return run(*args, capture_output=True, text=True).stdout


def native_platform():
    return {"Darwin": "macOS", "Windows": "Windows", "Linux": "Linux"}[platform.system()]


def architecture():
    machine = platform.machine().lower()
    return {"arm64": "ARM64", "aarch64": "ARM64", "x86_64": "X64", "amd64": "X64"}[machine]


def package_name():
    name = architecture()
    if native_platform() == "macOS":
        name = {"ARM64": "AppleSilicon", "X64": "Intel"}[name]
    return f"newKasina-{native_platform()}-{name}"


def read_metadata():
    with (MAC_RESOURCES / "Info.plist").open("rb") as handle:
        metadata = plistlib.load(handle)
    with (ROOT / "Cargo.toml").open("rb") as handle:
        version = tomllib.load(handle)["workspace"]["package"]["version"]
    metadata["CFBundleShortVersionString"] = version
    metadata["CFBundleVersion"] = os.environ.get("GITHUB_RUN_NUMBER", "1")
    return metadata


def check_resources():
    metadata = read_metadata()
    assert metadata["CFBundleIdentifier"] == "org.newkasina.NewKasina"
    assert metadata["CFBundleExecutable"] == "newKasina"
    assert metadata["CFBundlePackageType"] == "APPL"
    assert metadata["NSBluetoothAlwaysUsageDescription"].strip()
    assert metadata["LSMinimumSystemVersion"] == "13.0"
    assert metadata["NSHighResolutionCapable"] is True
    # Validate the PNG signature and dimensions without an imaging dependency.
    png = (MAC_RESOURCES / "newKasina.png").read_bytes()
    assert png[:8] == b"\x89PNG\r\n\x1a\n"
    assert int.from_bytes(png[16:20], "big") == 1024
    assert int.from_bytes(png[20:24], "big") == 1024
    assert (MAC_RESOURCES / "Read me first.html").is_file()
    print("Desktop packaging resources are valid.")


def build_icon(resources, temporary):
    iconset = temporary / "newKasina.iconset"
    iconset.mkdir()
    for logical_size in (16, 32, 128, 256, 512):
        for scale in (1, 2):
            pixels = logical_size * scale
            suffix = "@2x" if scale == 2 else ""
            name = f"icon_{logical_size}x{logical_size}{suffix}.png"
            run("sips", "-z", pixels, pixels, MAC_RESOURCES / "newKasina.png",
                "--out", iconset / name, stdout=subprocess.DEVNULL)
    run("iconutil", "--convert", "icns", "--output", resources / "newKasina.icns", iconset)


def verify_mac_app(app):
    contents = app / "Contents"
    with (contents / "Info.plist").open("rb") as handle:
        metadata = plistlib.load(handle)
    executable = contents / "MacOS" / metadata["CFBundleExecutable"]
    assert executable.is_file() and os.access(executable, os.X_OK)
    assert (contents / "Resources" / metadata["CFBundleIconFile"]).is_file()
    assert metadata["NSBluetoothAlwaysUsageDescription"].strip()
    run("plutil", "-lint", contents / "Info.plist")
    expected = {"ARM64": "arm64", "X64": "x86_64"}[architecture()]
    assert expected in output("lipo", "-archs", executable).split()
    # A bundle must not depend on Homebrew or files on the build machine.
    for line in output("otool", "-L", executable).splitlines()[1:]:
        dependency = line.strip().split(" (", 1)[0]
        if not dependency.startswith(("/System/Library/", "/usr/lib/")):
            raise RuntimeError(f"Unbundled non-system library: {dependency}")
    load_commands = output("otool", "-l", executable)
    advertised = tuple(map(int, metadata["LSMinimumSystemVersion"].split(".")))
    for command in re.split(r"Load command \d+", load_commands):
        if "cmd LC_BUILD_VERSION" in command:
            key = "minos"
        elif "cmd LC_VERSION_MIN_MACOSX" in command:
            key = "version"
        else:
            continue
        minimum = re.search(rf"^\s*{key}\s+(\d+\.\d+(?:\.\d+)?)$", command, re.M)
        if minimum and tuple(map(int, minimum[1].split(".")[:2])) > advertised:
            raise RuntimeError(f"Binary requires macOS {minimum[1]}, above the advertised minimum")
    run("codesign", "--verify", "--deep", "--strict", "--verbose=2", app)


def build_mac(binary_dir, staging, destination):
    app = staging / "newKasina.app"
    contents = app / "Contents"
    resources = contents / "Resources"
    resources.mkdir(parents=True)
    (contents / "MacOS").mkdir()
    executable = contents / "MacOS" / "newKasina"
    shutil.copy2(binary_dir / "kasina-app", executable)
    executable.chmod(0o755)
    with (contents / "Info.plist").open("wb") as handle:
        plistlib.dump(read_metadata(), handle, sort_keys=False)
    (contents / "PkgInfo").write_bytes(b"APPL????")
    shutil.copy2(MAC_RESOURCES / "Read me first.html", resources)
    with tempfile.TemporaryDirectory(prefix="newkasina-icon-") as temporary:
        build_icon(resources, Path(temporary))
    # Ad-hoc signing seals resources and supports Apple Silicon. It is not
    # Developer ID signing or notarization, and does not bypass Gatekeeper.
    run("codesign", "--force", "--sign", "-", "--timestamp=none", app)
    verify_mac_app(app)
    shutil.copy2(MAC_RESOURCES / "Read me first.html", staging)
    (staging / "Applications").symlink_to("/Applications", target_is_directory=True)
    dmg = destination / f"{package_name()}.dmg"
    run("hdiutil", "create", "-volname", "newKasina", "-srcfolder", staging,
        "-format", "UDZO", "-fs", "HFS+", "-ov", dmg)
    run("hdiutil", "verify", dmg)
    print(f"Created {dmg}")


def build_other(binary_dir, staging, destination):
    if native_platform() == "Windows":
        verify_windows_executable(binary_dir / "kasina-app.exe")
        shutil.copy2(binary_dir / "kasina-app.exe", staging / "newKasina.exe")
        instructions = (
            "Welcome to newKasina\n\n"
            "Extract this entire ZIP, then double-click newKasina.exe.\n"
            "The measurement service starts inside the app.\n"
            "Open the Measurement service panel for sensor connections.\n"
            "There is no separate server or installation step.\n\n"
            "This development build is not signed with a publisher certificate.\n"
            "Windows may ask you to confirm opening this downloaded app.\n"
        )
        archive_format = "zip"
    else:
        for executable in ("kasina-app", "kasina-service"):
            shutil.copy2(binary_dir / executable, staging)
        shutil.copy2(ROOT / "README.md", staging)
        shutil.copytree(ROOT / "docs", staging / "docs")
        instructions = (
            "Start kasina-service for a persistent acquisition service, then kasina-app.\n"
            "Alternatively, open kasina-app and start its local measurement service\n"
            "from the Measurement service panel. A running external service is reused.\n"
        )
        archive_format = "gztar"
    (staging / "START HERE.txt").write_text(instructions, encoding="utf-8")
    archive = shutil.make_archive(str(destination / package_name()), archive_format,
                                  root_dir=staging.parent, base_dir=staging.name)
    print(f"Created {archive}")


def windows_imports(executable):
    """Read the PE import table using only the standard library, on any host."""
    data = executable.read_bytes()
    if data[:2] != b"MZ":
        raise RuntimeError(f"Not a Windows executable: {executable}")
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe:pe + 4] != b"PE\0\0":
        raise RuntimeError(f"Missing PE header: {executable}")
    section_count = struct.unpack_from("<H", data, pe + 6)[0]
    optional_size = struct.unpack_from("<H", data, pe + 20)[0]
    optional = pe + 24
    magic = struct.unpack_from("<H", data, optional)[0]
    directory_offset = {0x10B: 96, 0x20B: 112}.get(magic)
    if directory_offset is None:
        raise RuntimeError(f"Unsupported PE optional header: {magic:#x}")
    imports_rva, imports_size = struct.unpack_from("<II", data, optional + directory_offset + 8)
    sections = optional + optional_size

    def offset(rva):
        for index in range(section_count):
            section = sections + index * 40
            virtual_size, virtual_address, raw_size, raw_address = struct.unpack_from("<IIII", data, section + 8)
            if virtual_address <= rva < virtual_address + max(virtual_size, raw_size):
                position = raw_address + rva - virtual_address
                if position >= len(data):
                    break
                return position
        raise RuntimeError(f"PE import points outside a file section: {rva:#x}")

    if not imports_rva:
        return []
    modules = []
    for relative in range(0, imports_size, 20):
        descriptor = struct.unpack_from("<IIIII", data, offset(imports_rva + relative))
        if not any(descriptor):
            return modules
        name_start = offset(descriptor[3])
        name_end = data.index(b"\0", name_start)
        modules.append(data[name_start:name_end].decode("ascii"))
    raise RuntimeError("PE import table is missing its terminating entry")


def verify_windows_executable(executable):
    imports = windows_imports(executable)
    runtimes = [name for name in imports if re.match(r"(?:vcruntime|msvcp|msvcr|concrt)\d.*\.dll$", name, re.I)]
    if runtimes:
        raise RuntimeError(
            "The Windows app still requires a Visual C++ redistributable: " + ", ".join(runtimes)
            + ". Build with an explicit MSVC target and target-feature=+crt-static."
        )
    print("Windows executable does not require a separate Visual C++ redistributable.")


def smoke_test(staging, destination):
    report = (destination / "smoke-test.json").resolve()
    report.unlink(missing_ok=True)
    arguments = ["--smoke-test-seconds", "3", "--smoke-test-output", str(report)]
    if native_platform() == "macOS":
        # Test the actual download: mount it read-only and install a copy before
        # asking LaunchServices to open it, just like dragging into Applications.
        dmg = destination / f"{package_name()}.dmg"
        with tempfile.TemporaryDirectory(prefix="newkasina-dmg-smoke-") as temporary:
            workspace = Path(temporary)
            mountpoint = workspace / "disk-image"
            mountpoint.mkdir()
            installed = workspace / "Applications"
            installed.mkdir()
            app = installed / "newKasina.app"
            attached = False
            try:
                run("hdiutil", "attach", "-readonly", "-nobrowse", "-noautoopen",
                    "-mountpoint", mountpoint, dmg)
                attached = True
                # ditto preserves the executable modes, extended attributes,
                # resource forks, and signature sealed in the downloaded app.
                run("ditto", mountpoint / "newKasina.app", app)
            finally:
                if attached or mountpoint.is_mount():
                    try:
                        run("hdiutil", "detach", mountpoint)
                    except subprocess.CalledProcessError:
                        # This is our own read-only temporary mount; no app is
                        # launched from it, and no writes can be lost.
                        run("hdiutil", "detach", "-force", mountpoint)
            verify_mac_app(app)
            # Keep the installed copy alive until the application has exited.
            run("open", "-W", "-n", app, "--args", *arguments, timeout=60)
    else:
        executable = "newKasina.exe" if native_platform() == "Windows" else "kasina-app"
        run(staging / executable, *arguments, timeout=60)
    result = json.loads(report.read_text(encoding="utf-8"))
    print(json.dumps(result, indent=2))
    if not result.get("success") or result.get("samples_received", 0) <= 0 or result.get("ui_frames", 0) <= 0:
        raise RuntimeError("Packaged app did not successfully render and receive simulated samples")
    if not result.get("service_instance") or result["service_instance"] != result.get("embedded_instance"):
        raise RuntimeError("Packaged app did not connect to its own isolated measurement service")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary-dir", type=Path, default=ROOT / "target" / "release")
    parser.add_argument("--output-dir", type=Path, default=ROOT / "dist")
    parser.add_argument("--check", action="store_true", help="Validate resources without packaging")
    parser.add_argument("--smoke-test", action="store_true", help="Test an already-packaged app")
    args = parser.parse_args()
    check_resources()
    if args.check:
        return
    destination = args.output_dir.resolve()
    staging = destination / ".staging" / package_name()
    if args.smoke_test:
        smoke_test(staging, destination)
        return
    if staging.exists():
        shutil.rmtree(staging)
    staging.mkdir(parents=True)
    commit = os.environ.get("GITHUB_SHA", "local development build")
    (staging / "BUILD.txt").write_text(
        f"Commit: {commit}\nPlatform: {native_platform()} {architecture()}\n"
        "Development build; no verified publisher signature or notarization.\n",
        encoding="utf-8",
    )
    if native_platform() == "macOS":
        build_mac(args.binary_dir.resolve(), staging, destination)
    else:
        build_other(args.binary_dir.resolve(), staging, destination)


if __name__ == "__main__":
    main()
