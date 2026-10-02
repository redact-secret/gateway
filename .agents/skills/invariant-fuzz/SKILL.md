---
name: invariant-fuzz
description: Property-based fuzzing of the gateway's fail-closed request invariants with proptest (or cargo-fuzz) against a fake upstream. Generates malformed, hostile, and boundary JSON bodies and headers and reports any counterexample with its seed. Use when asked to fuzz the gateway or check its invariants ("invariant-fuzz", "/invariant-fuzz 20000", "fuzz classify").
---

# invariant-fuzz

Break the gateway's invariants with generated inputs. Report counterexamples, not opinions.

## Orient first

The repo is in scaffolding. Find what exists: `graft skeleton` on the config, boundary, protocol, core-bridge, and transport modules, and `ls tests/ fuzz/ 2>/dev/null`. Fuzz only what is implemented. For an invariant whose code is absent, write "not implemented yet" rather than a verdict.

## Setup

- Prefer `proptest` for in-process properties through the crate's own entry points. Use `cargo-fuzz` only for byte-level parsers (framing, JSON admission) and only when asked. If neither is a dev-dependency, ask before adding one, and pin it exactly. Do not add a second property library.
- Drive the full pipeline with a fake upstream bound to loopback that records every byte it receives. Never use a real provider or a real credential. Use a stub or a synthetic core configuration where a unit needs a scanner, and a live core pin only for end-to-end properties.
- Properties live beside existing tests in `tests/` (name them `*_prop.rs` or `*.fuzz.rs`). Ask before committing new files.
- Run: `cargo test --locked --test <name>`. Set the run count with `PROPTEST_CASES=N` (default 2000; use the argument as `N`) and print the seed or failure-persistence file on failure.

## Invariants

Generators: JSON with duplicate keys, deep nesting, huge arrays, long strings, invalid UTF-8 and lone surrogates (`\ud800`), JSON escapes (`A`) and Unicode (ZWJ emoji, RTL, combining, Cf characters, Korean text), unknown fields at every nesting level, wrong types, NaN-like and out-of-range numbers, bodies at and just past every configured limit, a synthetic planted secret placed at random positions and split points, and headers with duplicates, folding, hop-by-hop names, `Content-Length` and `Transfer-Encoding` conflicts, and absolute-form or odd targets.

1. **No upstream bytes on rejection.** For any request that is rejected for any reason, the fake upstream receives zero body bytes and no connection that carries the request (`ARCHITECTURE.md` § Request state machine).
2. **Inspect-before-forward.** Any forwarded body is the transformed body, with a recomputed length, never the original. No planted synthetic secret appears in the bytes the fake upstream receives, for any position, split, or JSON-escaped form.
3. **Decoded-text inspection.** A secret hidden by JSON escaping or Unicode escapes is caught the same as the literal form. Text outside inspected fields is rejected or validated, never passed as free text.
4. **Unknown and unclassified fields reject.** Any unknown field at any depth, or any value that fails its structural contract, rejects the request. Duplicate keys and invalid UTF-8 reject.
5. **Structure preserved.** For accepted requests, keys, types, array order, and non-inspected values are unchanged and the output is valid JSON. No text replacement runs over the serialized body.
6. **Incomplete inspection rejects.** Truncation, finding-count exhaustion, detector failure, and limit hits (depth, node count, inspected text, findings, output size) reject. They never forward the remainder unscanned.
7. **Limits hold at the boundary.** Bodies at the limit behave per contract and one byte or node over rejects before unbounded allocation. Memory stays inside the configured aggregate budget under concurrent hostile inputs.
8. **Fixed safe errors.** Every gateway-generated error has a stable code from the documented taxonomy and contains no fragment of the request, headers, or planted secret, in body, headers, logs, or telemetry.
9. **Header handling.** Hop-by-hop headers and the local caller token are stripped, only allowlisted headers are forwarded, the provider credential appears only on the configured upstream request and never in a response, log, or error, and the destination never derives from caller input.
10. **Configuration.** Unknown config fields and invalid values reject at startup. No generated configuration makes the listener non-loopback without the documented explicit setting.
11. **Panics.** No input causes a panic, abort, or unbounded loop on untrusted data. Report any `unwrap`/index panic the generator reaches.

## Output

For each violated invariant: the invariant, the seed or persisted-failure path, the shrunk counterexample with values replaced by fixture names, and the observed vs expected result. End with `N cases, seed S: all invariants held` or the violation count, and list the invariants skipped as not implemented.

## Rules

- Synthetic values only. Never print plaintext from a failing case; name the fixture instead.
- Do not change product code. Propose a regression test for each confirmed violation.
- Committed property tests stay fast and deterministic under `cargo test`. Keep the default case count small in committed files and raise it only from the command line.
