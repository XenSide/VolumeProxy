# volume-proxy

Makes your volume knob or mixer control whichever app is currently in focus, automatically. Point any controller at `volume-proxy.exe` once, and from that moment turning the knob always affects the foreground app — no manual reassignment needed when switching between a game, a browser, a music player, or anything else. Runs silently in the background with no UI and near-zero resource usage, as it is entirely event-driven and never polls.

## Example use case: Stream Deck Neo

Elgato's Volume Control plugin supports targeting the foreground application, but that feature is only compatible with the Stream Deck+. On the Stream Deck Neo, you can only target a fixed application.

With volume-proxy running, you point the plugin at `volume-proxy.exe` once and never touch it again. From then on, the Neo's touch strip controls whichever app is currently in focus, with the dial staying in sync with its actual volume as you switch between apps.

## Features

- Works with any volume controller that can target a named audio session: hardware knobs, Elgato Wave Link, audio mixers, scripts
- Foreground app volume is applied instantly on focus switch
- Controller display stays in sync with the actual app volume at all times
- No UI, no taskbar presence — runs silently in the background
- Gracefully handles exclusive-mode audio devices with automatic retry

## How it works

A silent audio stream is opened in WASAPI shared mode and registered as an active audio session. Any volume controller — hardware knob, software mixer, script — can target this session by name. When the session receives a volume change event via `IAudioSessionEvents::OnSimpleVolumeChanged`, the delta is computed against the last known value and forwarded to the foreground app's `ISimpleAudioVolume` through the session enumerator.

A `WinEventHook` on `EVENT_SYSTEM_FOREGROUND` handles focus changes: when you switch to a different app, the proxy session's volume is immediately synced to that app's actual volume so the controller's display stays accurate.

Re-entrant callbacks from the proxy's own `SetMasterVolume` calls are suppressed via a sentinel `GUID` passed as the event context, so the volume update loop never triggers itself.

The process is entirely event-driven. There is no polling loop anywhere — every wake-up is triggered by a real WASAPI buffer event or a window focus change. The only exception is a 30-second retry sleep when another application holds the audio device in exclusive mode.

## Building

```
cargo build --release
```
