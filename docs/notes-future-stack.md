# Future Stack Notes

## birdcage — subprocess sandboxing

**Crate:** `birdcage` (by Phylum.io)  
**Repo:** https://github.com/phylum-dev/birdcage  
**What it does:** Cross-platform sandboxing for subprocesses. Uses seccomp + namespaces on Linux, `sandbox-exec` on macOS. Can restrict filesystem access (read-only paths, no-access paths), network, env vars.

**Why it's relevant to gait:**  
Shell tool execution (`shell`, `write`, `edit`) currently runs with full user privileges. Birdcage would let us cage each tool invocation — e.g. allow read-only access to the workspace, write access only to the codebase directory, no network for shell commands.

**Fits into the permission system as a deeper layer:**
```
allow list  →  approves the tool call
birdcage    →  enforces OS-level limits on what the tool can actually do
```

Even with `--auto`, birdcage would prevent a runaway shell command from writing outside the codebase or making network calls.

**Integration point:** `tools/shell.rs` — wrap the `tokio::process::Command` spawn in a `Birdcage` sandbox configured from the workspace's `gait.jsonc` sandbox policy.

**When to add:** After the permission/allow system is solid. Birdcage is the enforcement layer; the allow system is the approval layer. Do approval first, enforcement second.
