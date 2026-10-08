# gray-videogen

Async text-to-video across FAL / DeepInfra / xAI with persisted job polling. Port of hermes' video_gen plugins.

A sidecar plugin for [gray](https://github.com/vstaln/gray).

## What it does

- `video_gen` tool: `{prompt, provider?:"auto", model?, seconds?:5, out?:"./video-<ts>.mp4", action?, job?}`
- Providers are probed in order under `provider:"auto"`:
  | provider | env key | submit | default model |
  |---|---|---|---|
  | fal | `FAL_KEY` | `queue.fal.run/<model>` (submit → poll status → fetch response) | `fal-ai/pixverse/v6/text-to-video` |
  | deepinfra | `DEEPINFRA_API_KEY` | `api.deepinfra.com/v1/openai/videos` | `PrunaAI/p-video` |
  | xai | `XAI_API_KEY` | `api.x.ai/v1/videos/generations` | `grok-imagine-video` |
- Per-provider model env override: `<PROVIDER>_VIDEO_MODEL` (e.g. `FAL_VIDEO_MODEL`).
- All three APIs are async and can exceed the host's ~30s tool TTL: after ~20s
  of polling the job is persisted to `~/.gray/videogen/jobs.json` and the reply
  is `job <id> still running`. Finish it later with
  `video_gen {action:"status", job:"<id>"}` (or `"fetch"`); bare
  `action:"status"` lists known jobs.
- `/videogen` shows which providers are configured and how many jobs are on file.

## Wire methods

`plugin/manifest`, `tool/call`, `command/run`, `plugin/shutdown` — protocol 1.1.
HTTP via `curl`; no crates beyond `serde_json`.

## Install

```sh
gray plugin install videogen
```

## Develop

```sh
cargo test
gray account check      # entry point + manifest handshake
gray account publish    # check → build → release → publish to the gray registry
```
