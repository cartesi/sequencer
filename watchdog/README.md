# Watchdog

The watchdog's Lua sources, its production wrapper (`sequencer-watchdog`), the
release image's `Dockerfile`, the vendored native modules (`third_party/`), the
tests, and the test guest (`test-guest/`). Its recipes are in
[`justfile`](justfile), run as `just watchdog <recipe>`.

- [Watchdog README](../docs/watchdog/README.md): how it works, commands,
  configuration, metrics, code map, and tests.
- [Incident runbook](../docs/watchdog/incident-runbook.md): what to do when it
  latches a divergence.
- [Local development](../docs/watchdog/getting-started.md): running it against
  a devnet.
