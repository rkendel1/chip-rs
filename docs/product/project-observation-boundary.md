# The project-observation boundary

Status: **design contract. Nothing in this document is implemented.** It defines where project
structure belongs so that a later capability can be added without moving agency into PAX or
repository structure into Chip. Where it describes what exists today, it was checked against the
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

**What Chip uses.** Chip uses PAX for exactly one thing: the `pax.test` capability, which runs
`pax --dir <project> --json test` and records PAX's `pax.execution-result.v1`. The structure
capabilities Chip offers (`project.list`, `project.search`, `project.read`, `project.git.*`) are
Chip's own (`chip-project`), not PAX. Chip uses no PAX project-structure observation.

**What PAX observes today** (PAX v0.3.0; PAX's own `docs/PAX_AUDIT.md` and `docs/PAX_BOUNDARY.md` are
the authority, and the code and tests win over prose). PAX is a file-reading inspector with a
delegation layer:

* It detects ecosystems and package managers from manifests and lockfiles, and reports declared
  dependencies and scripts. This is accurate for `package.json` and Cargo. PAX's own audit calls the
  Python and Compose handling unreliable line scanning.
* `pax graph` reports **declared dependency and workspace-member relationships** only. Checked by
  running it on this repository: nodes are `component`, `dependency` and `workspace-member`; edges are
  `runtime-dependency`, `development-dependency` and `workspace-member`; for Rust it is built from
  `cargo metadata --no-deps --offline`. The same crate appears as both a `dependency` node and a
  `workspace-member` node, which is a reminder that provenance matters even at this level.
* `pax reality` and `pax drift` report **layered presence checks**, not verified reality. In PAX's own
  words: no PAX output yet meets the bar to be called reality; `resolved` and `installed` mean "a
  lockfile / a conventional directory exists"; lockfile contents and installed packages are not read;
  `--live` observes nothing and the `runtime` layer is `unknown`; drift's lockfile check is a substring
  match; nothing is timestamped.
* It delegates `build`, `test`, `lint` and `typecheck` to the native tool, and interprets the result
  for Cargo tests only.
* It does **not** observe source code, symbols, files' contents beyond manifests, installed tool
  versions, environment variables, or Git state.

So PAX today describes a project's *declared packaging structure*. It is not a code or repository
structure graph. It does not establish which files implement a behavior, which symbol depends on
which, which tests exercise which symbol, which files a change affects, which part of a repository
matters to a goal, or what to look at first. Those are future capabilities, and none of them is
claimed here.

**What `chip-graph` is.** An experiment (`EXPERIMENT`, `FROZEN`) in this repository, and no product
component consumes it: `chip work` and `chip serve` do not use it. It does link into the `chip`
binary (`chip init`, `chip graph`, `chip slice`, `chip impact`), which `crates.md` records as a known
cost. It builds a deterministic Rust-only architecture graph (nodes: repository, crate, module,
file, symbol, test suite, binary target, config surface; edges: contains, imports, defines,
implements, tests, targets). Its `slice` and `impact` are narrower than their names suggest, and
section 6 describes them as they are.

## 4. The observation model (conceptual, not an API)

Project observation is a family of small, independently requestable, deterministic facts rather than
one graph:

| Category | Example fact | Today in PAX |
| --- | --- | --- |
| Filesystem structure | `src/foo.rs` exists | only manifests, lockfiles and a few conventional directories |
| Source structure | function `X` is defined in `src/foo.rs`; module `Y` is declared by `src/lib.rs` | no |
| Dependency structure | package `A` declares dependency `B`; `C` belongs to workspace `W` | yes, declared edges only |
| Import structure | module `X` imports module `Y` | no |
| Test relationships | test `T` references function `X` | no |
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

## 10. The question this leaves open, and the gate for building it

> What is the minimum deterministic project-observation contract Chip needs to advance work without
> asking a model to rediscover repository structure?

The initial contract should be a small number of composable observations, not a universal graph:
project identity, file existence and location, source and module structure, symbol and declaration
location, structural relationships, dependency relationships, test relationships, tooling and
configuration.

Before an implementation PR, it must answer the questions the constitution already requires:

1. **Hypothesis:** scoped deterministic observation reduces model calls, executions and context spent
   on rediscovery without changing what Chip lets the model make real.
2. **What would falsify it:** no measurable reduction on matched real-model tasks, or any observation
   that a model can alter or that exceeds its evidence.
3. **Authority introduced:** none to the model. The observation is runtime-requested and
   reality-derived like every other observation. Any new capability is declared, validated and
   executed by Chip like the current ones.
4. **Measurement:** the same tasks with and without the capability, same model, same runtime,
   comparing model calls, executions, tokens and verified outcomes, kept separate from
   safety counters (`AGENTS.md` sections 12 and 22).
5. **What must not change:** the capability authority boundary, the completion contract (`verified`
   still requires an independent predicate), `chip-core` free of PAX, and PAX deterministic.

This document does not itself measure anything. The runs that motivate it are small and mixed:
several failures came from the model or the provider, not from missing structure, so rediscovery
cost is a hypothesis to test, not a finding.

## 11. What this change does not do

It adds no PAX capability, does not move `chip-graph` into PAX, builds no index, symbol database,
semantic search or relevance scoring, adds no model call to PAX, implements no impact analysis,
changes no part of Chip's work loop, capability API, FX or Compute, adds no persistence or cache,
and fixes no wire schema or command name.

## 12. The invariant

> Project structure belongs to project observation, not agency.

```text
REALITY -> PAX -> CHIP -> FX -> (decision) -> CHIP -> COMPUTE -> evidence -> CHIP
 observe   observation  agency   reasoning               execution    reality
```

Observe first. Interpret second. Decide third. Execute fourth. Verify what actually happened.
