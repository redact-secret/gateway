# Qualification documents

| Document | What it is |
| --- | --- |
| [alpha1-qualification-report.md](alpha1-qualification-report.md) | The Alpha 1 reconciliation: every acceptance item and blocker of #22, #8, and #1 against evidence, the SDK results, candidate artifact results, architecture and performance qualification, unresolved blockers, residual risks, and explicit non-claims; and the dated Alpha 2 transport qualification subsection (#60: frozen status mapping, framing, SDK retry, duplicate-work and truncation matrix). |
| [alpha1-threat-control-map.md](alpha1-threat-control-map.md) | Threat-to-test map and adversarial evidence for the SECURITY.md threat table (#25). |
| [deployment-chain-evidence.md](deployment-chain-evidence.md) | Procedures and recorded evidence for the environment-specific controls (direct-upstream bypass, intermediary framing conformance, resolver answers, trust store and TLS interception), per deployment shape, with what is executed (CI, Docker) versus documented only (Kubernetes) (#44). |
| [alpha1-stage-timing.json](alpha1-stage-timing.json), [alpha1-stage-timing-ci-linux.json](alpha1-stage-timing-ci-linux.json) | The raw synthetic stage-timing and peak-memory runs quoted in the report (local macOS arm64 and a GitHub-hosted Linux runner; no payloads; host load recorded; both labelled provisional because neither host was quiet). |
| [../probes/core-bridge-probe.md](../probes/core-bridge-probe.md) | The core API and completeness probe (#5). |

How the SDK qualification works (a separate non-release test build, a scripted fake provider, pinned hash-locked SDKs) is [ADR 0020](../decisions/0020-sdk-qualification-test-build.md). To reproduce it, see "SDK qualification" in the root [README](../../README.md#development) and [CONTRIBUTION.md](../../CONTRIBUTION.md#test-harness-and-ci).
