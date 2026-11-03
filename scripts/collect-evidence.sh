#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/evidence"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/frame-evidence.XXXXXX")"
export FRAME_TARGET_DIR="${FRAME_TARGET_DIR:-$ROOT/target/frame-apps}"

cargo build --release --manifest-path "$ROOT/Cargo.toml" >/dev/null
FRAME="$ROOT/target/release/frame"
rm -rf "$OUT"
mkdir -p "$OUT"/{context,diff,recovery,repair,bench}

project() {
  rm -rf "$WORK/$1"
  mkdir -p "$WORK/$1"
  cp "$ROOT/examples/fieldwork/fieldwork.frame" "$WORK/$1/"
  cp -R "$ROOT/examples/fieldwork/acceptance" "$WORK/$1/"
  echo "$WORK/$1"
}

capture() {
  local file="$1"; shift
  set +e
  "$@" >"$file" 2>&1
  echo "exit=$?" >>"$file"
  set -e
}

P="$(project main)"
"$FRAME" --project "$P" check >"$OUT/check.txt"
"$FRAME" --project "$P" fmt --check >>"$OUT/check.txt"
"$FRAME" --json --project "$P" test >"$OUT/test-report.json"
"$FRAME" --project "$P" context fieldwork/action/complete_task >/dev/null
cp "$P/.frame/context/fieldwork.action.complete_task/context.md" "$OUT/context/complete_task.context.md"
cp "$P/.frame/context/fieldwork.action.complete_task/manifest.json" "$OUT/context/complete_task.manifest.json"
capture "$OUT/context/budget-exceeded.txt" "$FRAME" --project "$P" context fieldwork/action/complete_task --max-bytes 600
cp "$P/.frame/context/fieldwork.action.complete_task/manifest.json" "$OUT/context/budget-exceeded.manifest.json"
cp "$P/.frame/ir.json" "$OUT/ir.json"
cp "$P/.frame/graph.json" "$OUT/graph.json"
cp "$P/.frame/manifest.json" "$OUT/artifact-manifest.json"

D1="$(project diff-bound)"
perl -0pi -e 's/(input title \{\n      type = Text;\n      min_length = 1;\n      max_length = )200/${1}100/' "$D1/fieldwork.frame"
"$FRAME" --project "$D1" diff --base "$P" >"$OUT/diff/input-bound.txt"
"$FRAME" --json --project "$D1" diff --base "$P" >"$OUT/diff/input-bound.json"
D2="$(project diff-owner)"
perl -0pi -e 's/owner_field = owner;/owner_field = assignee;/; s/field owner \{/field assignee {/; s/owner: \@actor.id,/assignee: \@actor.id,/' "$D2/fieldwork.frame"
"$FRAME" --project "$D2" diff --base "$P" >"$OUT/diff/owner-policy.txt"
"$FRAME" --json --project "$D2" diff --base "$P" >"$OUT/diff/owner-policy.json"

R="$(project recovery)"
capture "$OUT/recovery/exhaustion-dev-output.txt" env FRAME_PORT=0 FRAME_FAULT=crash_on_start "$FRAME" --project "$R" dev
"$FRAME" --json --project "$R" status >"$OUT/recovery/status-after-exhaustion.json"
"$FRAME" --json --project "$R" incidents >"$OUT/recovery/incidents.json"
capture "$OUT/recovery/restart-refused.txt" "$FRAME" --project "$R" dev
capture "$OUT/recovery/acknowledged-still-exhausted.txt" env FRAME_PORT=0 FRAME_FAULT=crash_on_start "$FRAME" --project "$R" dev --acknowledge
"$FRAME" --json --project "$R" status >"$OUT/recovery/status-after-acknowledge.json"

S="$(project seeded)"
perl -pi -e 's/set = \{ completed: true \};/set = { completed: false };/' "$S/fieldwork.frame"
capture "$OUT/repair/01-test-catches-seeded-bug.txt" "$FRAME" --project "$S" test
INC="$("$FRAME" --json --project "$S" incidents | perl -MJSON::PP -e 'local $/; print decode_json(<STDIN>)->{incidents}[0]{incident_id}')"
cat >"$WORK/evil.json" <<EOF
{"schema":"frame-patch/0.1","base_source_sha256":"$(shasum -a 256 "$S/fieldwork.frame" | cut -d' ' -f1)","base_graph_sha256":"","affected_nodes":["fieldwork/action/complete_task"],"allowed_scope":["fieldwork/action/complete_task"],"edits":[{"file":"acceptance/complete_task.json","start":0,"end":1,"replacement":"{"}],"rationale":"tries to edit a protected test"}
EOF
capture "$OUT/repair/02-malicious-proposal-rejected.txt" "$FRAME" --project "$S" repair "$INC" --proposer "file:$WORK/evil.json"
SHA_BEFORE="$(shasum -a 256 "$S/fieldwork.frame")"
"$FRAME" --json --project "$S" repair "$INC" --proposer fixture >"$OUT/repair/03-validated-report.json"
SHA_AFTER="$(shasum -a 256 "$S/fieldwork.frame")"
printf 'before repair: %s\nafter repair:  %s\n' "$SHA_BEFORE" "$SHA_AFTER" >"$OUT/repair/04-running-source-untouched.txt"
HASH="$(perl -MJSON::PP -e 'local $/; print decode_json(<STDIN>)->{proposal}{candidate_hash}' <"$OUT/repair/03-validated-report.json")"
cp "$S/.frame/repair/$INC/candidates/$HASH.json" "$OUT/repair/candidate.json"
LAST_ATTEMPT="$(ls -d "$S/.frame/repair/$INC"/attempt-* | sort -V | tail -1)"
cp "$LAST_ATTEMPT/task.json" "$OUT/repair/task-record.json"
cp "$S/fieldwork.frame" "$WORK/seeded.frame"
printf '// unrelated edit\n' | cat - "$WORK/seeded.frame" >"$S/fieldwork.frame"
capture "$OUT/repair/05-stale-candidate-rejected.txt" "$FRAME" --project "$S" apply-repair "$INC" --candidate "$HASH"
cp "$WORK/seeded.frame" "$S/fieldwork.frame"
capture "$OUT/repair/06-explicit-apply.txt" "$FRAME" --project "$S" apply-repair "$INC" --candidate "$HASH"
capture "$OUT/repair/07-tests-after-apply.txt" "$FRAME" --project "$S" test
S2="$(project nosandbox)"
perl -pi -e 's/set = \{ completed: true \};/set = { completed: false };/' "$S2/fieldwork.frame"
"$FRAME" --project "$S2" test >/dev/null 2>&1 || true
INC2="$("$FRAME" --json --project "$S2" incidents | perl -MJSON::PP -e 'local $/; print decode_json(<STDIN>)->{incidents}[0]{incident_id}')"
capture "$OUT/repair/08-sandbox-unavailable.txt" env FRAME_SANDBOX=none "$FRAME" --project "$S2" repair "$INC2" --proposer fixture

if [ "${SKIP_BENCH:-0}" != "1" ]; then
  B="$(project bench)"
  "$FRAME" --project "$B" bench ${BENCH_ARGS:-} >"$OUT/bench/summary.txt"
  cp "$B/.frame/bench/report.json" "$OUT/bench/report.json"
  cp "$B/.frame/bench/runtime/samples.json" "$OUT/bench/runtime-samples.json"
fi

find "$OUT" -type f | sort | sed "s|$ROOT/||" >"$OUT/INDEX.txt"
echo "evidence written to $OUT"
