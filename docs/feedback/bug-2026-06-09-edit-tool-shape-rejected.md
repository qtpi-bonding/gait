# Bug: `edit` tool rejects all input shapes with "invalid type: map, expected a sequence"

**Status:** Confirmed by direct testing, 2026-06-09. Repro is 6 lines.
**Severity:** High. `edit` is the primary tool for in-place file mutation.
When it fails, agents fall back to `write` (whole-file rewrite) which is
unsafe for large files and clobbers concurrent edits.
**Affects:** The `edit` tool in the agent's tool list, specifically the
JSON shape of its `edits` parameter.

## What happens

Calling `edit` returns an error before the file is even read. The error
is identical across multiple input shapes:

```
error: invalid type: map, expected a sequence
```

## Reproduction

```bash
# Setup: write a 5-line file
write /tmp/edit-test.md with content:
  line one
  line two
  line three
  line four
  line five

# Read it back (auto-approved, no prompt)
read /tmp/edit-test.md
# → returns 5 lines with anchors like "1#WZ|line one"

# Try to edit line 1 from "line one" to "line ONE"
edit /tmp/edit-test.md {
  "edits": [
    { "op": "replace", "pos": "1#WZ", "lines": ["line ONE"] }
  ]
}
# → error: invalid type: map, expected a sequence

# Variants tried, all same error:
# - "lines" as a 1-element string array
# - "lines" as a 2-element string array
# - Including "end" parameter
# - Different anchors ("1#KH", "1#WZ")
# - Trailing comma vs no trailing comma
```

Six attempts, six identical errors. The file was never modified.

## What the tool's own schema says

The `edit` tool's description (from the tool-list) is:

> *"Edit a file using hashline-anchored line references from `read` output.
> Always `read` the file first — the `N#XY` prefix on each line is the
> anchor for this tool. Pass one or more edits; all refs are validated
> before any write, so a stale anchor (line changed since your read)
> returns a clear error with updated refs instead of silently corrupting
> the file."*

And the parameter description:

> *"List of edit operations. Each references lines by their `N#XY`
> anchors from the most recent `read` of this file. Ops: `"replace"`
> (swap lines), `"append"` (insert after), `"prepend"` (insert before).
> All refs are validated before any write."*

Reading this, the obvious JSON shape is:

```json
{
  "edits": [
    { "op": "replace", "pos": "1#WZ", "lines": ["line ONE"] }
  ]
}
```

— exactly what I sent. But it doesn't work.

## Hypothesis

The error says *"invalid type: map, expected a sequence."* The "map"
is a JSON object (key-value pairs); the "sequence" is a JSON array.
The error is talking about something the tool expected to be a
sequence but received a map.

My `edits` parameter *was* a sequence (a 1-element array). So the tool
is complaining about *some other* shape, not the `edits` array itself.
Likely candidates:

1. **`edits` is supposed to be a bare array, not wrapped in an
   object.** If the tool's parser sees `{"edits": [...]}` and unwraps
   one level, then `[...]` (a sequence) is what it actually wants at
   the top level. But the JSON Schema / tool wrapper requires the
   `{...}` envelope, so this would be a protocol mismatch.

2. **The `lines` field is supposed to be a sequence of sequences.**
   I tried `["line ONE"]` (one string in an array) and `["line ONE",
   "line two"]` (two strings). Maybe the schema wants
   `[["line ONE"]]` — array of arrays? The error didn't specify
   which field is the offender, so I can't tell from the message.

3. **The `pos`/`end` fields are supposed to be sequences.** I sent
   `"1#WZ"` (a string). Maybe they want `["1#WZ"]` (an array)?

The error message would be much more useful if it named the field.
"Expected `edits[0].lines` to be a sequence, got map" tells the agent
exactly what to fix. "Invalid type: map, expected a sequence" is
effectively "your input was wrong, good luck figuring out what."

## Why this matters

The whole point of the `edit` tool, as designed, is to enable
**surgical, validated, hashline-anchored** file modifications. The
validation-before-write guarantee is the headline feature — it
prevents the worst class of silent corruption bugs.

But the tool is unusable in its current state, which means:

- Agents (me, in this session) fall back to `write` for any
  non-trivial change. For a 5-line test file, that's fine. For a
  1500-line source file with concurrent edits, it's not.
- The hashline-anchor system is wasted engineering — it's correct
  in principle but unreachable in practice.
- The agent spends tokens diagnosing the tool failure when it
  should be spending them on the actual task.

## Suggested fix (smallest)

Three options, in increasing order of invasiveness:

1. **Add a complete JSON example to the tool description.** The
   description currently shows the *shape* (`"op": "replace", "pos":
   "...", "lines": [...]`) but not a *full* example. A worked
   example like the one in the "Reproduction" section above would
   have let me (and any agent) test against the documented shape
   first, and either succeed or get a more specific error.

2. **Improve the error message to name the field.** *"Invalid type
   in `edits[0].lines`: expected sequence, got map."* This is a
   one-line change in the validator and unblocks agents that
   *almost* have the right shape.

3. **Fix the parser to accept what its own description claims.**
   The bug may be in the tool's argument parser, not the agent.
   If `edits: [{...}]` is what the description shows and the parser
   rejects it, that's a parser bug. Test against a known-good input
   and ship the fix.

I'd ship #1 first as a one-line doc patch — it costs nothing and
might already be enough. #2 is a 10-line code change with high
value. #3 is needed only if #1 doesn't help (i.e. the schema really
does want something other than what the description shows).

## Reproduction in a fresh session

The repro is deterministic and short. To verify:

1. Start a new `gait` session.
2. `write` any small file (5+ lines so anchors have variation).
3. `read` the file.
4. `edit` with `{"edits": [{"op": "replace", "pos": "<line-1-anchor>",
   "lines": ["new content"]}]}`.
5. Observe the same error.

If the error message changes between this report and the time you
reproduce, the bug is being fixed piecemeal — please update this
doc with the new symptom.

## Related observations

- The `edit` tool's hashline-anchor validation feature is *not* what
  is broken here. The failure happens *before* validation — the
  parser rejects the input shape, so we never reach the anchor
  check. The validation logic is presumably fine; it's just
  unreachable.
- The `write` tool worked correctly throughout the session and is
  the workaround I used. But `write` cannot do partial edits and
  loses the file's git history (whole-file rewrite) in a way that
  `edit` (multi-line replace) would not.
- The `read` tool's anchor format (`N#XY|content`) is a clever
  design — line numbers + content hash means anchors survive
  renumbering after edits. But none of that matters if `edit`
  can't be called.

## Suggested doc location

This belongs in `docs/feedback/` alongside the prompt wishlist and
the permission-reject bug — same "things the agent noticed"
category, same format. Not in `docs/superpowers/plans/` since it's
not an implementation plan, and not in `docs/plans/` since those
appear to be design-spec files (not bug reports).
