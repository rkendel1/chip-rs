# chip.work-decision.v1 reply fixtures

Each `*.reply.txt` is the raw text a model returned (the contract payload only: no prompt,
no credentials, no provider metadata). `tests/work_decision_fixtures.rs` runs every file through
`ModelDecisionBoundary`: `valid-*` must become a request for `compute.selftest`, `rejected-*`
must be an error.

Provenance:
- `valid-reference.reply.txt`, `valid-fenced.reply.txt`: hand-written from the contract. They are
  NOT captures of a real model.
- To add a real capture: `CHIP_TEST_REAL_MODEL=1 chip --test-real-model-work --print-reply`, save the
  text under "Model reply:" as `valid-real-<provider>.reply.txt`. If a real provider returns
  something the parser rejects, save it here and decide deliberately; do not loosen the parser
  just to make it pass.
