# VolumeProxy
Creates a silent dummy WASAPI audio session that appears as a real audio endpoint to any volume controller, hardware or software (Elgato Wave Link, audio mixers, scripts, etc.). When something adjusts that session's volume, the delta is intercepted and applied to whichever app is currently in focus instead. The proxy session stays in sync with the foreground app's actual volume, so the controller's display always reflects reality.

The process is entirely event-driven via WASAPI callbacks and a WinEventHook, with no polling loop. The only exception is a 30-second retry sleep when another application holds the audio device in exclusive mode.

This means the process uses **0% CPU and 1MB of RAM total**

# Example Usage
Elgato's Volume Control plugin supports targeting the foreground application, but that feature is only compatible with the Stream Deck+. On the Stream Deck Neo, you can only target one fixed application at a time.
With volume-proxy running, you point the plugin at volume-proxy.exe once and never touch it again. From then on, the Neo controls whichever app is currently in focus, with the graphic staying in sync with its actual volume as you switch between apps.
