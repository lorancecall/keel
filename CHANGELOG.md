# Changelog

## 0.1.38

- The tray panel is placed again whenever its size changes. It was placed with the size it had
  when first shown; at 200 % scale (4K) the window grew afterwards and its bottom ran off the
  screen under the taskbar.

## 0.1.37

- Game mode can also keep the main display only: the other displays are turned off while a game
  runs and Windows' saved layout comes back when it exits (GPU apps > Games > Game mode). The
  change is not saved in Windows' display settings, so a crash or sign-out leaves the normal
  layout; a marker file lets the tray put it back after a restart.
- Integer scaling can no longer be switched from the tray menu or the panel while a game runs.
  Turning it off put the desktop mode back under the running game, whose mouse then mapped to
  the wrong size.
- AMD: integer scaling is left as it is while a game runs. The driver applies a scaling change
  only at the next mode change, so following the game's mode did nothing visible (a 1440p game
  stayed small in black borders on a 4K monitor) and the display re-init upset the game's mouse.
  Run games at an exact divisor of the native mode (1080p on 4K) for a sharp full screen, or
  switch integer scaling off before starting a game at another size. NVIDIA keeps following.

## 0.1.36

- Local AI is now a tab of GPU apps: eGPU memory stays on top, with Games and Local AI tabs
  below (the side menu no longer has a Local AI item; `--tab local-ai` opens the tab).
- The main window opened from the tray or the tray panel comes to the front instead of behind
  the active window.
- Parts of the main window could stay see-through or show an old page after a display or
  monitor change (the software renderer only redraws what changed); the whole window is now
  redrawn after such changes, when it becomes active and every 10 seconds.
- Scroll bars get their own room at the right of a page instead of covering the cards.

## 0.1.35

- AI / game mode also starts again a startup app whose startup item is off (On-demand apps
  turns a followed app's item off while it runs, e.g. OneDrive).

## 0.1.34

- Game mode: app families picked on the Apps page (Game mode box) are switched off while a game
  runs, on the eGPU or the built-in GPU, and come back when it exits (startup apps start again).
  A game is what the GPU apps page counts as one (game folders and programs); the switch is on
  that page. AI mode and game mode share the work: an app in both lists stays off until both end.
- While a game runs, a model loaded again (another program asked Ollama) is unloaded within
  10 seconds; before, models were unloaded only when the game started.

## 0.1.33

- AI mode starts the startup apps it closed (OneDrive and others started from a Run key or the
  Startup folder) again when the model unloads, with their own command line. Apps that were not
  running before are not started.

## 0.1.32

- Network works with any wired adapter and on any Windows language: Wi-Fi and LAN are found by
  adapter type, not by name ("Wi-Fi", "Realtek USB GbE"). The Network page has a LAN picker
  (any wired adapter, or one of those attached); virtual adapters (Hyper-V, VPN, Bluetooth,
  Wi-Fi Direct) are never taken for a LAN or Wi-Fi.
- A Radeon feature the driver refuses to switch on (Fluid Motion Frames on some mobile GPUs) is
  hidden from the list after the first try and named below it; `keel egpu amd-feature <name> on`
  tries again (e.g. after a driver update).
- Old names left from before Keel are gone from the code; the Legion Space marker is only
  `atiadlxx.dll.keel`.

## 0.1.31

- A new install changes no network settings: Network routing starts off with no VPNs (it used to
  start on, with a Tailscale kill switch). Existing settings stay as they are.
- The build writes the installer's SHA-256 next to it; the README explains checking it, the
  SmartScreen / antivirus warnings of an unsigned installer, and everything Keel may change.
- MIT license.

## 0.1.30

- A card that reports no power maximum is scaled to exactly the highest draw measured (the stress
  test's maximum), not that plus a margin.
- Radeon features: switching one on that turns another off (Chill and Anti-Lag) saves both as the
  driver shows them, so the next attach does not set both; the card says so.

## 0.1.29

- External GPU gauges show their scale at the arc's ends. A card that reports no power maximum
  (AMD without power tuning) is scaled to the highest draw seen; until a stress test has run, the
  power gauge shows "?" and the Stress test card says so (it used to assume 300 W).
- The graph's note is shorter (same scales as the gauges).
- Opening the window straight on a page (tray menu, `--tab`) loads its data, e.g. the AMD
  features in the Tuning card.

## 0.1.28

- Any eGPU is recognised: a display device Windows expects to be unplugged (USB4, Thunderbolt,
  OCuLink dock) gets its own profile when it first attaches, at the driver defaults (no limits).
  No GPUs are built into the default configuration any more; existing profiles stay as they are.

## 0.1.27

- Local AI: the AI mode card lists the picked apps in a table (App, Now, Note), like On-demand apps.

## 0.1.26

- AI mode is faster: one request for all apps, families the service already manages first, one
  scan for the rest (it was one scan per app, about 90 s for six apps).
- AI mode also closes apps the service cannot switch (installed per user, such as Zoom, or with
  nothing machine-wide, such as Store apps): the tray closes their running processes itself.

## 0.1.25

- AI mode: the tray sends its changes to the service one after another and waits while the service
  is busy (sent all at once, they failed with "All pipe instances are busy" and nothing switched off).

## 0.1.24

- AI mode apps are picked on the Apps page (AI mode box), from any app family, not only on-demand
  ones. The service switches them off while a model is loaded (services, tasks, startup items and
  running processes, originals saved first) and puts them back when it unloads, also after a
  reboot. An app opened again during the load is left alone; protected families cannot be picked.
- Apps page: Keep off / Follow app / Back to normal show their result instead of "Service running".

## 0.1.23

- Home: the memory graph starts with this session's samples instead of an empty chart; widgets can
  be reordered in the Widgets menu (arrows), and the External GPU page can put Tuning above Live.
- The app icon colour and style now also apply to the Start menu and desktop shortcuts (set by the
  service).
- Local AI: AI mode switches chosen on-demand apps off while an Ollama model is loaded and back on
  when it unloads (done by the tray).
- NVIDIA tuning: a memory clock ceiling on GPUs that support it (RTX 30 and newer).

## 0.1.22

- NVIDIA 3D settings: after a change the service restarts NVIDIA's display container, which otherwise
  kept the old settings and the NVIDIA Control Panel closed right after opening. "Default" now writes
  the driver's default value instead of deleting the setting from the global profile.
- The 3D settings sit under the stress test, so the Tuning card's two columns are about the same height.
- The main window opens at 980 x 720 and pages fit its width instead of being cut off on the right
  (long notes wrap, the graph's range buttons sit under it, narrower table columns).

## 0.1.21

- NVIDIA 3D settings in the Tuning card: max frame rate, low latency, vertical sync, power
  management mode, on the driver's global profile (only NVIDIA GPUs, so only the eGPU). Read with
  `keel egpu nv-3d`; written by the service (offered settings and values only).
- NVIDIA helper services are stopped only after no NVIDIA GPU has been seen for 2 minutes: after
  boot the eGPU shows up tens of seconds after the service starts.
- Tuning number boxes: no clear button over the digits; Enter or leaving the box sets the value
  and the focus leaves; clicking the card's empty space leaves it too.

## 0.1.20

- Tuning values can be typed in (Enter sets them; kept within the allowed range), for NVIDIA and AMD.
- The stress test summary opens from a Report button once a run has finished.

## 0.1.19

- External GPU: tuning and the stress test share one full-width card (limits on the left, the
  stress test on the right).

## 0.1.18

- AMD eGPU tuning on the External GPU page: power limit and maximum clock when the card's driver
  offers them, back to factory values, and switches for the Radeon features (Anti-Lag, Chill,
  Boost, Image Sharpening, Enhanced Sync, Radeon Super Resolution, Fluid Motion Frames). Only the
  eGPU is changed (its own driver's ADLX, matched by device ID). The tray sets them again on each
  attach. CLI: `keel egpu amd-tune`, `keel egpu amd-feature`.
- Changing the tuning starts the record of highest clocks again.
- The network rule that keeps Tailscale on Wi-Fi is now a setting (`tailscale_wifi_only` in
  net.toml; on for existing configs, off for new ones).
- Release builds carry no build-machine paths.

## 0.1.17

First published version.

- Service, tray and main window; installer with upgrade in place.
- eGPU: per-device AMD driver install with a Windows Update block, NVIDIA power limit and clock
  ceiling with a thermal watchdog, telemetry for NVIDIA and AMD, stress test, per-GPU profiles,
  NVIDIA helper services that follow the eGPU (opt-in), Adrenalin restart when it hangs.
- Displays: resolution, main display and integer scaling for eGPU monitors and the built-in
  screen; game hand-over for integer scaling; keep-or-revert for every mode change.
- Legion Space: kept on the iGPU and, while the built-in screen is the main display, on the
  built-in screen; its resolution changes go to the built-in screen.
- Local AI: eGPU memory, installed Ollama models that fit, loaded models with their GPU / RAM
  split, unload, end Ollama; models unloaded when a game starts or the eGPU is unplugged.
- GPU apps: installed Steam, Epic and GOG games with an eGPU switch each, other programs set to
  the eGPU, game folders and programs.
- Memory: app families, on-demand apps (keep off / follow app), memory history with leak and early
  signs, per-session samples.
- Network (optional): internet over Wi-Fi or LAN, a side and kill switch per VPN, Wi-Fi beside the LAN.
- Tray panel on a left click, app icon colours, light and dark themes.
