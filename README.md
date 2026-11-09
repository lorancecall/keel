<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/keel-256-light.png">
    <img src="assets/keel-256.png" width="112" alt="Keel logo">
  </picture>
</p>

<h1 align="center">Keel</h1>

A Windows companion for **eGPU users on handheld PCs**, built on and for the Lenovo Legion Go
(Ryzen Z1 Extreme). Keel keeps an NVIDIA or AMD external GPU, the built-in screen, Legion Space
and a small amount of RAM working together: one service, a tray icon and a main window.

<p>
  <img src="docs/screenshots/external-gpu.png" width="49%" alt="External GPU page: live power, temperature and clock history, gauges, stress test">
  <img src="docs/screenshots/displays.png" width="49%" alt="Displays page: built-in screen and eGPU monitor with resolution and integer scaling">
</p>
<p>
  <img src="docs/screenshots/tray-panel.png" width="260" alt="Tray panel: free memory, eGPU, network and integer scaling at a glance">
</p>

## What it does

- **NVIDIA and AMD eGPUs, without DDU.** An AMD eGPU gets its own driver, bound to its exact
  hardware ID, so the iGPU keeps the vendor driver; Windows Update is kept from replacing it.
  Each removable eGPU (USB4, Thunderbolt, OCuLink) has its own profile, starting at the driver
  defaults. NVIDIA's helper services can be stopped while no NVIDIA GPU is present (opt-in).
- **Legion Space and the eGPU get along.** Two options (External GPU page, tray menu):
  *Legion Space on iGPU* runs it on the iGPU and sends a resolution picked in it to the built-in
  screen; works with any main display. *Legion Space on built-in screen* keeps its windows there;
  works only while the built-in screen is the main display. See [Scenarios](#scenarios).
- **Integer scaling on every screen.** eGPU monitors and the built-in screen. Pick an exact
  divisor (1920 x 1080 on 4K, 1280 x 800 on the 2560 x 1600 panel) and every pixel stays sharp.
  Mode changes ask to keep them and revert after 15 seconds.
- **GPU apps.** Games from Steam, Epic and GOG, plus your own folders and programs, each with a
  switch to run on the eGPU. A game set to the eGPU opens on the eGPU monitor, and Legion Space
  popping up at game start is sent back. Game mode switches apps you pick off while a game runs
  and can keep only the main display.
- **Local AI.** Free eGPU memory, which Ollama models fit in it, what is loaded now (GPU vs RAM
  split, unload, end Ollama). Models are unloaded when a game starts or the eGPU is unplugged.
  AI mode switches apps you pick off while a model is loaded.
- **On-demand apps.** Services, scheduled tasks, startup entries and updaters, grouped by vendor
  and install folder into *families*. "Keep off", or "Follow app" (on while the app runs).
  Everything is recorded first and put back with "Back to normal" or on uninstall.
- **eGPU tuning.** Live power, temperature, clocks, usage and VRAM; power limit, clock ceilings
  and 3D settings on NVIDIA; Radeon features; a short stress test. Only what the card supports is
  shown. A hung AMD Adrenalin is restarted (automatically or from the tray).
- **Also:** memory history with leak detection, a tray panel, light and dark themes, and optional
  network routing (internet over Wi-Fi or LAN, a side and kill switch per VPN; any wired adapter,
  any Windows language). Network routing is off until you set it up.

## Scenarios

| Setup | What happens |
|---|---|
| eGPU monitor is the **main display** | Apps start on the eGPU. *Legion Space on iGPU* works. Legion Space lays its windows out on the main display and puts them back if moved, so *Legion Space on built-in screen* has no effect here (it can be switched off, not on). |
| Built-in screen is the **main display**, eGPU monitor extended | Both options work. Apps set to the eGPU render there; games set to the eGPU are moved to the eGPU monitor. |
| Built-in screen **off** | *Legion Space on iGPU* works; its windows follow the main display. |
| **No eGPU** | eGPU pages show "not attached"; the Local AI tab notes that models would only take system RAM; integer scaling on the built-in screen keeps working. |
| **Game running** | Ollama models are unloaded first. NVIDIA: integer scaling follows the game's mode and returns afterwards. AMD: the driver applies scaling only at a mode change, so it stays as set; run the game at an exact divisor. |
| **eGPU unplugged** | Ollama models are unloaded; the other eGPU settings wait for the next attach. |
| **Switching NVIDIA <-> AMD** | The new card's profile applies; the AMD driver stays bound to the AMD card only. |

## Install

1. Download `keel-setup-<version>.exe` and its `.sha256` file from [Releases](../../releases).
2. Check the hash:

   ```powershell
   Get-FileHash .\keel-setup-<version>.exe -Algorithm SHA256
   ```

   It must match the `.sha256` file and the release notes. If not, do not run the file.
3. Run the installer as administrator. It installs to `C:\Program Files\Keel`, registers the `Keel`
   service, starts the tray at logon and optionally adds `keel` to the PATH. Upgrades keep your
   settings.

```powershell
keel status        # service and features
keel help          # command-line tools (memory, eGPU, displays, network)
```

To build it yourself (Windows 11, Rust stable with MSVC, Inno Setup 6): `.\build.ps1` builds,
tests and writes `dist\keel-setup-<version>.exe` with its SHA-256.

### Windows warnings, and what Keel can do on your PC

The installer is **not code-signed**:

- **SmartScreen** shows "Windows protected your PC". "More info", then "Run anyway", only after
  the hash matched.
- **Defender or other antivirus** may flag it: an unsigned program that installs a service and
  changes system settings. Check the hash; if unsure, don't install, or build from source.

Keel runs a service as LocalSystem. All it may change:

| Part | What it may change | When |
|---|---|---|
| eGPU | power limit and clock ceilings (NVIDIA), Radeon features, NVIDIA 3D settings, display modes and scaling, the AMD eGPU's driver binding, a Windows Update block for that driver | only what you set; profiles start at the driver defaults |
| eGPU | stops / starts NVIDIA's helper services | only if you turn that option on |
| On-demand apps / AI and game mode | service start types, scheduled tasks, startup entries of app families you pick; ends their processes | only families you pick; never Windows core, GPU drivers, the handheld vendor's or security software |
| Network | interface metrics, routes, firewall rules for VPN kill switches | only after you turn Network on and set it up |

- Every change is recorded first and put back by "Back to normal", by turning the feature off, or
  by uninstalling Keel.
- Drivers are never deleted. No kernel drivers, no code injected into other programs. The service
  does fixed jobs only; it never runs a path or command it is sent.
- No telemetry, no update checks. The only connection is to Ollama on your own PC
  (`127.0.0.1:11434`).

**Use at your own risk.** Tested on one Legion Go (Ryzen Z1 Extreme) with an NVIDIA RTX 2080 Ti
and an AMD RX 7600M XT on a USB4 dock; other eGPUs and handhelds are untested. Keel is not
affiliated with Lenovo, AMD, NVIDIA or Microsoft.

## Layout

```
apps/keel      keel.exe: command line and the service entry
apps/tray      keel-tray.exe: tray icon, menu, display hand-over, memory sampling
apps/gui       keel-gui.exe: main window and tray panel (Slint)
crates/core    shared: paths, logs, settings, control pipe, app icon
crates/ram     memory: scan, families, on-demand apps, memory history
crates/egpu    eGPU: drivers, limits, telemetry, displays, integer scaling, games, Legion Space
crates/net     network routing
crates/service the Keel service
installer      Inno Setup script
```

See [CHANGELOG.md](CHANGELOG.md) for releases.

## License

[MIT](LICENSE). No warranty.

The user interface is made with [Slint](https://slint.dev), used under the Slint Royalty-free
Desktop, Mobile, and Web Applications License 2.0.

[![Made with Slint](https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-light.svg)](https://slint.dev)
