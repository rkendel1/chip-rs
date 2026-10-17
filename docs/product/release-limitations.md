# Limitations of the first Rust Chip release

This page is the contract for what Rust Chip does **not** do in its first release, and what it does instead
when a request needs it. It records release-scope decisions, not equivalence with the Zig `fx` agent. The
behavior comparison, with the differences kept as evidence, lives in `rkendel1/fx`:
`docs/rebuild/parity/agent-loop.v1.json` and `docs/rebuild/evidence/agent-parity.json`.

## Read this first: retrying may repeat external effects

**RETRYING A TURN MAY REPEAT EXTERNAL EFFECTS.** Chip does not guarantee exactly-once side effects, and
persistence does not make a capability idempotent.

* If a `chip` process is killed, or a work is interrupted, while a capability that changes the world (a file
  write, a Git operation, a command PAX runs) is running or has just finished, Chip does **not** know whether
  the effect happened as far as a *new* run is concerned. Nothing is replayed automatically.
* If you start the same work again, the model may ask for the same capability again and Chip will execute it
  again. There is no guard that recognizes "this was already done".
* Chip does **not** resume an interrupted model response. An interrupted turn stays uncommitted; asking again
  starts from the beginning. (The Zig agent can resume a paused response; Rust Chip cannot.)
* The shipped `chip` and `chip serve` do not persist turns at all. The `chip-turns` library does, and reports
  one of `Committed`, `NotCommitted` or `Uncertain`; **a caller must treat `Uncertain` as "may or may not have
  happened"** and must not assume either.

**What callers must do.** Before retrying interrupted work, account for effects that may already have happened:
inspect the project (`git status`, `git diff`), prefer goals whose effects are naturally repeatable (for
example "make the tests pass", which a repeat does not harm) over ones that are not, and do not wire an
automatic retry around a work that can change something you cannot afford to change twice. A rerun that
repeats an effect is not a bug in the retry; it is this limitation.

Accepting this limitation for the first release is the repository owner's decision (2026-10-16). It is
conditional on this warning staying visible: it is in the `README`, in `RELEASE_NOTES.md` (shipped in the
release artifact), and in `chip work`'s usage text, and the fx parity gate checks that these notices exist.

## Behaviors Rust Chip does not provide

Every one of these is *unsupported in the first release*, with the explicit runtime behavior below. None is
silently ignored, and none ever reports a successful completion because of it. "Verified" means observed
against the built binary or pinned by a named test; "by design" means stated from the code and not separately
exercised.

| Behavior (parity id) | Category | What happens when a request needs it |
| --- | --- | --- |
| Permission modes `ask` / `auto` / `yolo` (`permission_modes`) | compatibility | There are no modes: one fixed policy, declared capabilities only. `--yolo`, `--auto`, `--prompt-permissions` are refused as unexpected arguments with exit 2 before anything runs (**verified**). |
| Provider-native tool definitions and several tool calls in one reply (`provider_native_tool_loop`) | core runtime | Chip uses one text decision per reply. A reply that is not exactly one clean decision object is rejected, never repaired, and the work ends (pinned by `output_that_is_not_one_clean_json_object_is_rejected_and_never_repaired`); FX refuses tool definitions in the request. |
| Continuing silently after a tool (`silent_tool_continuation`) | core runtime | Every execution result is an observation returned to the model; there is no mode that proceeds without asking the model (**by design**). |
| Pausing at the step limit and continuing (`step_limit_pause`) | core runtime | The work ends as `limit_reached`, which is not a completion; there is no pause, `--continue-recovery` or `--resume` (those flags are refused, **verified**). |
| Image attachments and vision (`images_and_vision`) | product surface | `--image` is refused (exit 2, **verified**). Chip sends text only; FX refuses non-text parts. |
| Subagents, MCP tools, skills, memory, web tools (`subagents_mcp_skills_memory`) | product surface | Chip declares no such capabilities. A model request for one is an undeclared capability: the decision is rejected, nothing executes, and the work ends as failed without a retry (pinned by the parity scenarios `unknown_tool` and `shell_not_executed`). |
| Interactive approval of a denied action (`interactive_permission_prompts`) | safety | Chip never prompts. A request that is not authorized is refused and the work fails closed; it is not parked waiting for a human (**by design**, pinned by the same scenarios). |
| Cancellation between a tool result and the next request (`cancel_after_tool`) | safety | Cancelling the process ends it; no completion is reported and no further tool runs. The service's `POST /v1/work/{id}/cancel` abandons a model call in flight (with its delivery certainty) but does not interrupt an execution in flight. An effect that already ran stays done (see above). |
| A publish failure after the work already ran (`persistence_failure_after_work_ran`) | safety | The shipped binary persists nothing, so there is nothing to fail. With `chip-turns`, the result is `NotCommitted` or `Uncertain`, never `Committed`, and the work is not reported as durably recorded. This path is covered by `chip-turns` tests and could not be injected through the binary, so it is **not** part of the parity run. |
| An idempotence guard for re-run work (`duplicate_side_effects_guard`) | safety | None exists. See the warning at the top. |

## Other known limitations (unchanged by this page)

* The native FX provider has no HTTP proxy or custom CA support (`native-provider.md`, issue #35).
* Ollama's own `/api/chat` API is not supported; use its OpenAI-compatible endpoint.
* OpenRouter has no live evidence; this is waived for the migration and is not a claim that it works.
* Security review of the Zig agent (fx `docs/rebuild/security/ZIG_EXTERNAL_WRITE.md`) covers only what it
  says it tested; symlinks, races, the TUI/ACP paths and source-versus-binary consistency are untested.
