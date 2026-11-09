#!/usr/bin/env python3
"""Prepare the SoundSight 7.1 virtual speaker driver sources.

The driver is Microsoft's SimpleAudioSample (Windows-driver-samples,
audio/simpleaudiosample, MS-PL) at the commit pinned in WDS_COMMIT, with a small,
checked set of edits applied:

* the speaker endpoint is 8 channels (7.1 surround) instead of stereo,
* the fake microphone endpoint is not registered,
* the device, service and endpoint names and the hardware ID are SoundSight's.

Every edit must match exactly once, so a change upstream fails the build
instead of silently producing a different driver.

Usage: prepare.py <Windows-driver-samples checkout> <output dir>
The output dir gets the patched sample plus the WDK NuGet wiring
(packages.config + Directory.Build.props) copied from the same checkout.
"""

import re
import shutil
import sys
from pathlib import Path

HARDWARE_ID = r"ROOT\SoundSight71"


def edit(path: Path, pattern: str, repl: str, encoding: str = "utf-8", count: int = 1) -> None:
    raw = path.read_bytes()
    text = raw.decode(encoding)
    new, n = re.subn(pattern, repl, text)
    if n != count:
        sys.exit(f"prepare.py: expected {count} match(es) of {pattern!r} in {path}, found {n}")
    path.write_bytes(new.encode(encoding))


def main() -> None:
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    wds = Path(sys.argv[1]).resolve()
    out = Path(sys.argv[2]).resolve()
    sample = wds / "audio" / "simpleaudiosample"
    if not sample.is_dir():
        sys.exit(f"prepare.py: {sample} not found")

    if out.exists():
        shutil.rmtree(out)
    shutil.copytree(sample, out / "simpleaudiosample")
    for f in ("packages.config", "Directory.Build.props", "LICENSE"):
        shutil.copy2(wds / f, out / f)

    # Make the WDK NuGet imports independent of the directory msbuild runs from.
    props = out / "Directory.Build.props"
    edit(props, r"(?<=['\"])packages\\", r"$(MSBuildThisFileDirectory)packages\\", count=10)

    src = out / "simpleaudiosample" / "Source"

    # --- 7.1 speaker endpoint ------------------------------------------------
    wav = src / "Filters" / "speakerwavtable.h"
    edit(wav, r"(#define SPEAKER_DEVICE_MAX_CHANNELS\s+)2\b", r"\g<1>8")
    edit(wav, r"(#define SPEAKER_HOST_MAX_CHANNELS\s+)2\b", r"\g<1>8")
    # WAVEFORMATEXTENSIBLE: channels, rate, avg bytes/s, block align, bits.
    edit(
        wav,
        r"(WAVE_FORMAT_EXTENSIBLE,\s+)2,(\s+)48000,(\s+)192000,(\s+)4,(\s+)16,",
        r"\g<1>8,\g<2>48000,\g<3>768000,\g<4>16,\g<5>16,",
    )
    edit(wav, r"KSAUDIO_SPEAKER_STEREO", "KSAUDIO_SPEAKER_7POINT1_SURROUND")
    edit(src / "Filters" / "speakertoptable.h", r"KSAUDIO_SPEAKER_STEREO", "KSAUDIO_SPEAKER_7POINT1_SURROUND")

    # --- no microphone endpoint ----------------------------------------------
    edit(
        src / "Filters" / "minipairs.h",
        r"#define g_cCaptureEndpoints \(SIZEOF_ARRAY\(g_CaptureEndpoints\)\)",
        "#define g_cCaptureEndpoints 0 // SoundSight: render-only device",
    )
    # The capture loop now never runs; the WDK builds with C4296 as an error.
    edit(
        src / "Main" / "adapter.cpp",
        r"(\n)([ \t]*)(for \(ULONG i = 0; i < g_cCaptureEndpoints;)",
        r"\g<1>\g<2>#pragma warning(suppress: 4296)\n\g<2>\g<3>",
    )

    # --- INF: names, hardware ID, no mic interfaces ---------------------------
    inx = src / "Main" / "SimpleAudioSample.inx"
    enc = "utf-16"  # the sample's INX is UTF-16 with a BOM
    edit(inx, r"ROOT\\SimpleAudioSample", HARDWARE_ID.replace("\\", "\\\\"), enc)
    edit(inx, r"AddInterface=%KSCATEGORY_\w+%,\s*%KSNAME_\w+MicArray1%[^\r\n]*\r?\n", "", enc, count=5)
    edit(inx, r"(?s)\[SIMPLEAUDIOSAMPLE\.I\.WaveMicArray1\].*?(?=;=+\r?\n; render interfaces)", "", enc)
    edit(inx, r"AddService=SimpleAudioSample,", "AddService=SoundSight71,", enc)
    edit(inx, r"KmdfService = SimpleAudioSample,", "KmdfService = SoundSight71,", enc)
    strings = {
        "ProviderName": "SoundSight",
        "MfgName": "SoundSight",
        "MsCopyRight": "Microsoft Corporation and SoundSight contributors",
        "SIMPLEAUDIOSAMPLE_SA.DeviceDesc": "SoundSight 7.1 Virtual Speakers",
        "SimpleAudioSample.SvcDesc": "SoundSight 7.1 Virtual Speakers Driver",
        "SIMPLEAUDIOSAMPLE.WaveSpeaker.szPname": "SoundSight 7.1",
        "SIMPLEAUDIOSAMPLE.TopologySpeaker.szPname": "SoundSight 7.1",
    }
    for key, value in strings.items():
        edit(inx, rf"(?m)^({re.escape(key)}\s*=\s*)\"[^\"]*\"", rf'\g<1>"{value}"', enc)

    print(f"Prepared SoundSight 7.1 driver sources in {out}")


if __name__ == "__main__":
    main()
