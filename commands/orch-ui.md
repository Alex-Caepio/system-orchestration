---
description: Launch the embedded loopback WebAssembly orchestration workbench (`orch-engine serve`)
argument-hint: "[--project <path>] [--port 8766]"
---

Launch or inspect the local WebAssembly orchestration workbench for `$ARGUMENTS`.

Run the embedded workbench CLI against the target repository:

```bash
"${CLAUDE_PLUGIN_ROOT:-.}/bin/orch-engine" serve --project "<target-repo>" --port 8766
```

Wait for the JSON startup banner (`{"ok":true,"url":"http://127.0.0.1:..."}`) and share the loopback URL with the user. Use `--state-dir` to isolate state directories per project if running multiple instances.
