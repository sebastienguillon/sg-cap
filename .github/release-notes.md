## Install

Download `sgcap.exe`, move it to a permanent folder, and run it. An icon appears
in the notification area; press `Ctrl+Shift+4` to capture a region. Enable
"Start SgCap with Windows" in the settings to keep it available after a reboot.

Requires 64-bit Windows 10 (version 1903 or later) or Windows 11. No runtime
or installer is needed.

## This binary is not code-signed

SgCap is currently distributed **unsigned**. Expect the following:

- Windows SmartScreen shows "Windows protected your PC" the first time you run
  it. Click **More info**, then **Run anyway**.
- Your browser or antivirus may flag the download. Screen-capture tools that
  register global hotkeys are a common false positive.

The executable attached here was built by GitHub Actions from the tagged source
of this repository; the workflow run is linked from this release. You can check
the download against `sgcap.exe.sha256`:

```
certutil -hashfile sgcap.exe SHA256
```

Third-party license notices for the bundled libraries are in
`THIRD_PARTY_NOTICES.md`.
