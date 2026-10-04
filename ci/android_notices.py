import hashlib
import json
import pathlib
import re
import subprocess
import struct


ROOT = pathlib.Path(__file__).resolve().parent.parent
OUTPUT = ROOT / "target/android-notices/assets/notices"


def run(*command: str) -> str:
    return subprocess.check_output(command, cwd=ROOT, text=True)


def font_notices() -> bytes:
    parser = (ROOT / "wezterm-font/src/parser.rs").read_text()
    records = []
    for name in sorted(set(re.findall(r'font!\("../../assets/fonts/([^\"]+)"\)', parser))):
        font = (ROOT / "assets/fonts" / name).read_bytes()
        tables = struct.unpack_from(">H", font, 4)[0]
        offset = next(struct.unpack_from(">I", font, 12 + index * 16 + 8)[0]
                      for index in range(tables) if font[12 + index * 16:16 + index * 16] == b"name")
        _, count, strings = struct.unpack_from(">HHH", font, offset)
        notices: dict[int, set[str]] = {0: set(), 1: set(), 13: set(), 14: set()}
        for index in range(count):
            platform, _, _, field, length, start = struct.unpack_from(">HHHHHH", font, offset + 6 + index * 12)
            if field not in notices:
                continue
            raw = font[offset + strings + start:offset + strings + start + length]
            notices[field].add(raw.decode("utf-16-be" if platform in {0, 3} else "mac_roman"))
        record = {"file": name, "sha256": hashlib.sha256(font).hexdigest(),
                  "copyright": sorted(notices[0]), "family": sorted(notices[1]),
                  "license": sorted(notices[13]), "license_url": sorted(notices[14]),
                  "missing_embedded_fields": [field for field, values in
                                              (("copyright", notices[0]), ("license", notices[13]),
                                               ("license_url", notices[14])) if not values]}
        if name == "SymbolsNerdFontMono-Regular.ttf":
            record["upstream_notice"] = "bundled/assets/fonts/NOTICE_NERD_FONT_SYMBOLS.md"
        records.append(record)
    return (json.dumps(records, indent=2, sort_keys=True) + "\n").encode()


def main() -> None:
    metadata = json.loads(run("cargo", "metadata", "--locked", "--format-version", "1"))
    closure = set()
    for target in ("aarch64-linux-android", "x86_64-linux-android"):
        tree = run("cargo", "tree", "--locked", "--target", target, "-p", "wezterm-android",
                   "--edges", "normal,build", "--prefix", "none", "--format", "{p}")
        closure.update(tuple(line.split()[:2]) for line in tree.splitlines())
    texts: dict[str, bytes] = {
        "fonts.json": font_notices(),
        "obligations.txt": b"This inventory does not establish legal completeness or distribution authority.\n"
                           b"Static libssh LGPL obligations, including applicable source and relinking obligations,\n"
                           b"are not discharged solely by bundling COPYING. Legal review remains required.\n",
    }
    packages = []
    for package in sorted(metadata["packages"], key=lambda item: (item["name"], item["version"], item["id"])):
        if (package["name"], "v" + package["version"]) not in closure:
            continue
        directory = pathlib.Path(package["manifest_path"]).parent
        files = [file for file in directory.rglob("*") if file.is_file()
                 and re.match(r"^(licen[sc]e|copying|copyright|notice)([._-]|$)", file.name, re.I)
                 and not any(part in {"target", ".git"} for part in file.relative_to(directory).parts)]
        if package.get("license_file"):
            files.append(directory / package["license_file"])
        if not files and directory.is_relative_to(ROOT):
            files = [ROOT / "LICENSE.md"]
        if not files:
            for parent in directory.parents:
                if parent.name in {"checkouts", "src"}:
                    break
                files = [file for file in parent.iterdir() if file.is_file()
                         and re.match(r"^(licen[sc]e|copying|copyright|notice)([._-]|$)", file.name, re.I)]
                if files:
                    break
        receipts = []
        for file in sorted(set(files)):
            content = file.read_bytes()
            digest = hashlib.sha256(content).hexdigest()
            name = f"texts/{digest}.txt"
            texts[name] = content
            receipts.append({"path": str(file.relative_to(directory)) if file.is_relative_to(directory) else file.name,
                             "asset": name, "sha256": digest})
        packages.append({"name": package["name"], "version": package["version"],
                         "source": package["source"], "license": package["license"],
                         "authors": package["authors"], "notices": receipts})
    for file in [ROOT / "LICENSE.md", *(ROOT / "assets/fonts").glob("LICENSE*"),
                 *(ROOT / "assets/fonts").glob("NOTICE*"),
                 *(ROOT / "deps").rglob("LICENSE*"), *(ROOT / "deps").rglob("COPYING*"),
                 ROOT / "deps/freetype/freetype2/docs/FTL.TXT"]:
        if file.is_file():
            texts["bundled/" + str(file.relative_to(ROOT))] = file.read_bytes()
    OUTPUT.mkdir(parents=True, exist_ok=True)
    expected = set(texts) | {"index.json"}
    for file in OUTPUT.rglob("*"):
        if file.is_file() and str(file.relative_to(OUTPUT)) not in expected:
            file.unlink()
    for name, content in texts.items():
        path = OUTPUT / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
    (OUTPUT / "index.json").write_text(json.dumps(packages, indent=2, sort_keys=True) + "\n")
    print(f"notices: {len(packages)} dependency records, {len(texts)} license files")
    missing = [package["name"] for package in packages if not package["notices"]]
    if missing:
        print(f"Upstream packages without separate license text, metadata retained: {', '.join(missing)}")
    for font in json.loads(texts["fonts.json"]):
        if font["missing_embedded_fields"]:
            print(f"Font {font['file']} missing embedded fields: {', '.join(font['missing_embedded_fields'])}; "
                  f"upstream notice: {font.get('upstream_notice', 'not available')}")


if __name__ == "__main__":
    main()
