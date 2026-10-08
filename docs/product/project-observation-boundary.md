# The project-observation boundary

Status: **the boundary, and its first consumer.** `project.observe` (section 3) implements it for Rust
project structure through `pax.observation.v1`. This document defines where project structure belongs so
that agency does not move into PAX and repository structure does not move into Chip. Where it describes what exists today, it was checked against the
code or by running the tools; those statements say which.

> PAX observes project reality. Chip decides what to do with it. FX reasons about what observations
> mean.

## 1. The problem

A model working in a project can discover structure only through `project.list`, `project.search`
and `project.read`. It can end up re-deriving, call after call, things a deterministic tool could
state once: what exists, what a manifest declares, how one file relates to another. An informal
real-model validation of the current runtime saw one model spend its whole execution budget on
repeated, near-identical searches without a read. That run is not committed to this repository, it
is one model on one fixture, and its failures mixed model behavior with missing structure, so it
suggests that rediscovery costs something; it does not show that this boundary would remove the
cost. Section 10 says how that is to be measured before anything is built.

The earlier `chip-graph` experiment showed that some project structure can be represented
deterministically. The lesson to keep is that such structure is *observation*. It should not become
a Chip-owned subsystem:

```text
wrong:                      right:

  Chip                        PROJECT -> PAX -> Chip -> FX
   └── owns structure                    observation  agency  reasoning
```

## 2. Authorities

| Layer | Owns | Does not own |
| --- | --- | --- |
| **PAX** | deterministic observation of project artifacts, with provenance | goals, plans, relevance, correctness, goal satisfaction, retries, memory, approvals, any model call |
| **Chip** | agency: which observation to request, validation, the work loop, evaluation, limits, terminal state | what the project contains (it asks), what an observation means (it asks FX) |
| **FX** | the cognition boundary: model and provider substitution | observation, execution, authority |
| **Compute** | execution and execution-level evidence | project structure, agency |

The model's judgment may *select* an observation to request; it never *supplies* one. An
observation, like every other piece of evidence (`AGENTS.md` sections 4 and 11), is created by the
runtime from reality, never from a model's words.

`chip-core` must not depend on PAX. Today it does not: `chip-core` depends on `fx-core` and
nothing that touches PAX, and `chip-pax` is the adapter that depends on `chip-core`. A future
observation capability follows the same shape: `chip-core` -> environment contract -> capability ->
PAX adapter. PAX stays independently installable and independently testable (Chip locates a `pax`
on the search path and pins a minimum release).

## 3. What exists today

**Pinned release.** Chip is built, tested and benchmarked against PAX **0.4.1**, tag `v0.4.1`, commit
`674f3b3143874d1a692aca103b33f89da31a82ac` (CI installs exactly that tag and checks that commit;
`chip_pax::PINNED_PAX` records it). `project.observe` is explicit (not in the default capability set; `CHIP_ENABLE_PROJECT_OBSERVE=true`) and requires PAX >= 0.4.1: the version is the
compatibility contract, and support is never detected by running an unsupported command. An older PAX
fails with `PAX observation requires >= 0.4.1; found <version>`. `pax.test` keeps its own minimum (0.3.0).

**What Chip uses PAX for.**

| Capability | PAX surface | Contract |
| --- | --- | --- |
| `pax.test` | `pax --dir <root> --json test` | `pax.execution-result.v1`, PAX >= 0.3.0 |
| `project.observe` | `pax --dir <root> --json observe --scope <s> --max-files 50 --max-bytes 1048576 --max-facts 600` | `pax.observation.v1`, PAX >= 0.4.1 |

`project.observe` consumes **only** `pax.observation.v1`. It does not use `pax graph`, `pax info`,
`pax reality` or `pax drift`, and reads no field the contract does not define. The structure
capabilities Chip also offers (`project.list`, `project.search`, `project.read`, `project.git.*`) are
Chip's own (`chip-project`) and are unchanged.

**What PAX observes (0.4.1, `docs/PAX_OBSERVATION.md` in PAX is the authority).** Bounded structure
facts, each with provenance (`declared`, `observed`, `resolved`; `verified` is never emitted): artifacts
that exist, workspace members, declared dependencies (aggregated across members, **not** attributed to a
package), and for Rust: crate, module, source file, declaration and test-attribute facts, with typed
diagnostics for what it could not establish. It does **not** observe other languages, `impl` blocks
and their methods, imports, calls, types, `cfg` evaluation, `#[path]` modules, or anything macro-generated.
It says nothing about relevance, impact, safety or correctness.

**What Chip adds, and does not delegate.** The scope grammar and project-root containment are checked
by Chip **before** PAX is started (lexically, and through the filesystem, refusing every symlink);
Chip fixes every bound; PAX's document is parsed strictly and the observation is rendered by Chip, not
passed through; every artifact a fact names is re-walked and a fact behind a symlink is omitted (the
observation becomes `partial` and says so). PAX's own containment is a second layer, not a substitute.

**Earlier PAX.** In PAX 0.3.0, `pax graph` attached the workspace-wide union of declared dependencies to
every workspace member, with evidence naming a manifest that did not contain the declaration (found by
testing an adapter against `cargo metadata`). PAX 0.4.1 fixed that. Chip does not use `graph` for this
capability regardless: the observation contract is the only surface consumed.

**What `chip-graph` is.** An experiment (`EXPERIMENT`, `FROZEN`) in this repository, and no product
component consumes it: `chip work` and `chip serve` do not use it. It does link into the `chip`
binary (`chip init`, `chip graph`, `chip slice`, `chip impact`), which `crates.md` records as a known
cost. Its `slice` and `impact` are narrower than their names suggest, and section 6 describes them as
they are. The PAX observation covers a useful subset of the structural facts it established, which is
evidence that the experiment informed the right boundary; it does not make `chip-graph` a dependency.

## 4. The observation model (conceptual, not an API)

Project observation is a family of small, independently requestable, deterministic facts rather than
one graph:

| Category | Example fact | Today in PAX |
| --- | --- | --- |
| Filesystem structure | `src/foo.rs` exists | manifests, lockfiles, conventional directories, source files reached by `mod` |
| Source structure | function `X` is defined in `src/foo.rs`; module `Y` is declared by `src/lib.rs` | yes, Rust only (`pax observe`) |
| Dependency structure | package `A` declares dependency `B`; `C` belongs to workspace `W` | membership yes; declared dependency names yes, aggregated across the workspace (not attributed to a package) |
| Import structure | module `X` imports module `Y` | no (PAX does not observe `use`) |
| Test relationships | test `T` references function `X` | no; PAX states only that a test attribute is present (`test.declared`) |
| Tooling and configuration | the selected build tool and why; manifests; lockfiles | yes |

A test relationship is structure, not proof: "test `T` references `X`" does not mean "`T` shows `X`
is correct". Test structure and test execution are different observations, and execution evidence
stays separate.

## 5. Provenance is part of the boundary

An observation that does not say where it came from is not enough for an agent that must reason
safely. A future observation should be able to say: what was observed (subject, relation, value),
from which artifact and location, by what method, and when, where that is meaningful. The schema is
deferred.

The rule is that **an observation must never become a stronger claim than its evidence supports.**
In particular these stay distinct, and no layer may silently promote one to the next:

| State | Means | Example |
| --- | --- | --- |
| **declared** | a project file says so | `Cargo.toml` lists dependency `X` |
| **observed** | seen directly on this machine now | the directory exists |
| **resolved** | a resolver's result was read | the lockfile pins `X` at a version (PAX does not read lockfile contents today) |
| **verified** | execution established it | the tests ran and passed |

"`Cargo.toml` declares `X`" is not "`X` is installed", which is not "the application used `X`
successfully". The existing PAX audit findings stay visible: its field names `reality`, `resolved`
and `installed` mean layered *presence* observations, and a Chip consumer must read them that way
until PAX's own bar for "reality" is met.

## 6. Structure is not relevance

A structural graph can establish `A -> B -> C`. It cannot establish that `C` is relevant to a goal.
For the goal "fix expired session cleanup", PAX may report that `session.rs` calls `cleanup.rs`,
which calls `storage.rs`; it must not conclude "modify `cleanup.rs`". That needs goal semantics,
current work state, observed behavior, history, constraints and judgment, and so it belongs above
PAX: *PAX exposes structure; Chip and FX determine relevance.*

There is a third authority in the existing experiment, and it should be named so it is not confused
with the other two. In `chip-graph`:

* **`slice`** is not a structural neighborhood. It is exactly the nodes a capability *declared*
  through an external catalog (`chip.capabilities.v1`), plus the edges among them. "Nothing is
  traversed, ranked, guessed or asked of a model." The relevance is **declared by whoever wrote the
  catalog** (a human, an application, CI), not computed.
* **`impact`** resolves changed paths to file nodes, adds the modules and crates containing them, and
  reports a capability as impacted when one of its declared nodes is in that set. The containment
  step is structural observation; the "which nodes matter to this capability" step is again the
  declared catalog.

So the old experiment is decomposed as follows, and not moved wholesale:

| Piece | What it is | Where it belongs |
| --- | --- | --- |
| containment and structural neighborhood of an observed artifact (file -> module -> crate, symbol -> its definers, test -> referenced symbols) | deterministic observation | PAX, if and when it is needed |
| "these nodes matter to this capability" | declared relevance, owned by the catalog's author | outside PAX and outside the model |
| "this part of the project matters to this goal / this change" | interpretation | Chip and FX |

Contract rule: the observation surface exposes no semantic conclusions. Names like
`relevant_to_goal`, `recommended_change`, `likely_fix`, `safe_to_modify` or `goal_impact` are outside
it.

## 7. What PAX must not become

PAX stays deterministic. It must not call a model, infer intent, choose an implementation strategy,
produce a plan, rank candidate fixes, decide goal satisfaction or change safety, decide whether a
human is required, retry failed work, keep agent memory or goals, or own approvals. A deterministic
analysis may live in PAX. An interpretation moves upward.

## 8. Relationship to the rest of Chip

* **FX** should not be used to discover repository structure that a deterministic tool can state.
  Observation first, then reasoning over observed facts, then a decision. That also keeps the model
  from being the source of repository topology (`AGENTS.md` section 15: a model may select; it does
  not supply).
* **The Decision Frontier** (the planned split of what is known, what is at the frontier and what is
  unknown) is **not implemented or specified anywhere in this repository today.** This document uses
  it only as a stated direction: a deterministic observation can move a fact from unknown to known
  ("`A` imports `B`"); it does not perform the next inference ("so change `B`"), which is an
  agency step.
* **Historical execution evidence** is separate from structure. "Function `X` calls `Y`" is a
  structural observation. "Changing `Y` fixed that failure last time" is history, owned by an
  evidence authority, not PAX.
* **Attn and AppPort** are not integrated and this design does not add them (`AGENTS.md` section 14).

## 9. Observation is demand-driven and bounded

A capability to observe project structure does not mean every work cycle builds a repository model.
Observation should be scoped, bounded, deterministic, demand-driven, provenance-bearing,
independently testable and reusable only where that is justified. A plain verification needs no
structure at all and must pay nothing for it. This is Chip's performance rule applied here: no
always-on index, no mandatory full-repository analysis.

The aim is not to maximize what the model knows. It is to reduce rediscovery:

```text
now:   model -> search -> read -> infer -> search -> read -> infer
later: Chip requests a scoped observation -> PAX returns structured facts -> FX reasons only where
       reasoning is needed
```

## 10. The gate: this capability is kept only if it measures

The hypothesis is that scoped deterministic observation reduces model calls, executions and context
spent rediscovering repository structure, without changing what Chip lets the model make real. What
would falsify it, what must not change, and what is measured are in
[`observe-benchmark.md`](observe-benchmark.md), together with the results of the first matched
baseline-versus-treatment run. Those results, not the existence of the facts, decide whether
`project.observe` stays, narrows or goes.

What must not change, and is pinned by tests: the capability authority boundary (the model supplies one
validated `scope` and nothing else), the completion contract (`verified` still requires an independent
predicate; an observation never grounds an inspect answer, never satisfies a goal, and is never useful
work), `chip-core` free of PAX, and PAX deterministic and agency-free.

## 11. What this does not do

It adds no relevance scoring, ranking, impact analysis, index, symbol database, semantic search,
cache, persistence or watcher; no model call in PAX; no automatic substitution of `project.list`,
`project.search` or `project.read` (they are unchanged and the model chooses); no change to Chip's
work loop, completion or utility semantics, FX or Compute; and no `chip-graph` dependency. It does not
implement the Decision Frontier or escalation: an observation may reduce uncertainty, it does not
choose what happens next.

## 12. The invariant

> Project structure belongs to project observation, not agency.

```text
REALITY -> PAX -> CHIP -> FX -> (decision) -> CHIP -> COMPUTE -> evidence -> CHIP
 observe   observation  agency   reasoning               execution    reality
```

Observe first. Interpret second. Decide third. Execute fourth. Verify what actually happened.
