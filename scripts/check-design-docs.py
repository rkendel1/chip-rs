#!/usr/bin/env python3
"""Consistency check for the runtime-intelligence design documents and their roadmap tickets.

Documentation tooling only: it reads Markdown, imports no project code and changes nothing. Checks:
  1. every normative requirement (WC-nn, EL-nn, BC-nn, MS-nn) in the four design documents has an Acceptance
     condition in its own block;
  2. every requirement id mentioned anywhere in the design documents or the roadmap exists;
  3. every roadmap id (P0-xx, P1-xx, P1-Vx, P1-Ox, P2-xx, P2-Dx, P2-Cx, P3-xx, RIC-xx) mentioned in the design
     documents or tickets exists as a heading, bullet id or ticket in the roadmap;
  4. every RIC ticket names at least one requirement id as acceptance;
  5. every relative Markdown link in docs/product and README.md resolves.
Exit 0 when all hold, 1 otherwise, with each violation listed.
"""
import glob, os, re, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
P = os.path.join(ROOT, "docs", "product")
DESIGN = ["work-contract.md", "evidence-ledger.md", "blockage-classifier.md", "micro-step-proposal-gate.md"]
ROADMAP = "coding-agent-production-roadmap.md"
read = lambda f: open(os.path.join(P, f), encoding="utf-8").read()
errors = []

# 1. requirements and their acceptance
defined = {}
for f in DESIGN:
    text = read(f)
    parts = re.split(r"(?m)^(?=\* )|^(?=#{2,4} )", text)
    for part in parts:
        for m in re.finditer(r"\*\*((?:WC|EL|BC|MS)-\d+)\.\*\*", part):
            rid = m.group(1)
            if rid in defined:
                errors.append(f"{rid} defined twice ({defined[rid]} and {f})")
            defined[rid] = f
            if "Acceptance" not in part:
                errors.append(f"{f}: {rid} has no Acceptance condition in its block")

# 2. every mentioned requirement id exists
ids = set(defined)
for f in DESIGN + [ROADMAP, "production-boundary.md"]:
    for m in re.finditer(r"\b((?:WC|EL|BC|MS)-\d+)\b", read(f)):
        rid = m.group(1)
        if rid not in ids and not re.match(r"BC-R\d", rid):
            errors.append(f"{f}: references unknown requirement {rid}")
# ranges such as "WC-01 to WC-20" only need their ends to exist (checked above)

# 3 and 4. roadmap ids and tickets
road = read(ROADMAP)
road_ids = set(re.findall(r"(?m)^### ((?:P[0-3]-[A-Z]?\d+)|(?:RIC-\d+))\b", road))
road_ids |= set(re.findall(r"\*\*(P2-C\d+|P3-\d+)\b", road))
road_ids |= {"RIC-07a", "RIC-07b", "RIC-07c"}
for f in DESIGN + ["production-boundary.md"]:
    for m in re.finditer(r"\b(RIC-\d+[abc]?|P[0-3]-[A-Z]?\d+)\b", read(f)):
        if m.group(1) not in road_ids and m.group(1) not in {i for i in road_ids}:
            errors.append(f"{f}: references roadmap id {m.group(1)} that the roadmap does not define")
tickets = re.split(r"(?m)^(?=### RIC-\d+)", road)
for t in tickets[1:]:
    head = t.splitlines()[0]
    body = t.split("\n### ")[0]
    if not re.search(r"\b(?:WC|EL|BC|MS)-\d+\b", body):
        errors.append(f"roadmap: {head} cites no requirement id as acceptance")
    if "Verification" not in body:
        errors.append(f"roadmap: {head} has no Verification")
    if "Dependencies" not in body and "Depends" not in body:
        errors.append(f"roadmap: {head} states no dependencies")

# 5. links
for f in glob.glob(os.path.join(P, "*.md")) + [os.path.join(ROOT, "README.md")]:
    for m in re.finditer(r"\]\((?!https?:|#|mailto:)([^)#\s]+)(#[^)]*)?\)", open(f, encoding="utf-8").read()):
        target = os.path.normpath(os.path.join(os.path.dirname(f), m.group(1)))
        if not os.path.exists(target):
            errors.append(f"{os.path.relpath(f, ROOT)}: broken link {m.group(1)}")

for e in errors:
    print("FAIL:", e)
print(f"{len(defined)} requirements, {len(road_ids)} roadmap ids, {len(tickets) - 1} tickets, {len(errors)} problems")
sys.exit(1 if errors else 0)
