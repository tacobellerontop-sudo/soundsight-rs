# SoundSight 7.1 virtual speakers

An optional virtual output device, **"Speakers (SoundSight 7.1 Virtual Speakers)"**.
Set it as the Windows default output with a 7.1 speaker setup, and games render
full 7.1 into it. SoundSight then captures it with the same WASAPI loopback it uses
for any device, so you get all 8 directions without installing Voicemeeter or VB-Cable.

The pieces:

| Path | What it is |
|---|---|
| `driver/WDS_COMMIT` | Pinned commit of [microsoft/Windows-driver-samples](https://github.com/microsoft/Windows-driver-samples) |
| `driver/prepare.py` | Copies `audio/simpleaudiosample` (MS-PL) from that commit and applies SoundSight's edits: 8-channel 7.1 speaker endpoint, no microphone endpoint, SoundSight names, hardware ID `ROOT\SoundSight71`. Every edit must match exactly once, or the build fails. |
| `setup/` | `SoundSight71Setup.exe`: one file with the driver inside. It creates the root device and installs or removes the driver. Run `SoundSight71Setup.exe install` / `uninstall` (`--quiet` for scripts), or with no arguments to get a dialog. |
| `.github/workflows/ci.yml`, job `driver` | Builds the driver with the WDK NuGet packages, then builds the setup with the driver embedded. |

## Status: developer build only (test-signed)

Windows 10/11 with Secure Boot only loads kernel drivers that **Microsoft has signed**.
CI signs with a throw-away test certificate, so the CI artifact
(`SoundSight71-driver-TESTSIGNED`) only works on a PC with test signing turned on:

```
bcdedit /set testsigning on      (admin, then reboot; turn Secure Boot off if it refuses)
certutil -addstore root SoundSight71-TestCert.cer
certutil -addstore TrustedPublisher SoundSight71-TestCert.cer
SoundSight71Setup.exe install
```

**Anti-cheat (Vanguard, EAC, BattlEye, FACEIT) refuses to run games in test-signing mode**,
so this build is for development only. That's why it isn't published on the `latest` release.

## What it takes to ship it to players

1. **A legal entity** (company/LLC). EV code-signing certificates are only issued to
   registered organisations.
2. **An EV code-signing certificate** (DigiCert, Sectigo, GlobalSign, SSL.com...),
   about $250-600 per year. These come on a hardware token or as a cloud HSM.
3. **A Microsoft Partner Center account** in the Windows Hardware Developer Program
   (free). Registering requires signing with the EV certificate.
4. **Attestation signing** for each release: sign the driver `.cab` with the EV cert,
   upload it to Partner Center ("Submit new hardware", attestation), and download the
   Microsoft-signed package. That package goes into `SOUNDSIGHT_DRIVER_DIR` when
   building `SoundSight71Setup.exe`. Attestation-signed drivers load on Windows 10/11
   client editions with Secure Boot on, and anti-cheat accepts them.

Once (1)-(3) exist, the upload can be automated in CI with the Partner Center
hardware submission API.

## Known limits

* Windows 11 only for now (the sample's INF targets build 22000+).
* It's a sink: whatever is played into the device isn't heard anywhere yet. SoundSight
  needs a "play through to my headset" option (capture the virtual device, downmix
  to the real output) before this is useful day to day. That's the next step.
* 48 kHz, 16-bit, 8 channels, which is what Windows uses for 7.1 in shared mode.
