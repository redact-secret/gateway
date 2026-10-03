//! Smoke tests for the performance workload scaffolding (ADR 0008; issue #6).
//!
//! These check that the tool runs, covers the required shapes, and emits no payloads or
//! credential labels. They assert nothing about speed or memory.

mod support;

use std::time::Duration;

use support::leak::{BODY_MARKER, Markers};
use support::workloads::{self, Plan, Shape};

fn tiny() -> Plan {
    Plan {
        small: 2 * 1024,
        large: 64 * 1024,
        iterations: 3,
        threads: 3,
        sse_fragments: 3,
        sse_delay: Duration::from_millis(1),
    }
}

#[test]
fn payload_shapes_are_distinct_valid_json_of_the_requested_size() {
    for shape in [Shape::NoFindings, Shape::ManyFindings, Shape::LargeInput] {
        let body = workloads::payload(shape, 8 * 1024);
        assert!(body.len() >= 8 * 1024, "{shape:?}");
        assert!(serde_json::from_slice::<serde_json::Value>(&body).is_ok());
    }
    let many = String::from_utf8(workloads::payload(Shape::ManyFindings, 4096)).unwrap();
    let none = String::from_utf8(workloads::payload(Shape::NoFindings, 4096)).unwrap();
    assert!(many.matches("SYNTH-FINDING-").count() > 10);
    assert_eq!(none.matches("SYNTH-FINDING-").count(), 0);
    // Synthetic by construction.
    assert!(many.contains("NOT-A-CREDENTIAL"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_workloads_run_and_the_report_contains_no_payload_or_markers() {
    let lines = workloads::run_all(tiny(), Some("test-commit")).await;
    let rendered: String = lines.iter().map(|l| format!("{l}\n")).collect();

    // Header pins and required workload coverage.
    assert_eq!(lines[0]["kind"], "header");
    assert!(lines[0]["core_pin"].as_str().unwrap().starts_with("0.1.0"));
    for workload in [
        "no_findings",
        "many_findings",
        "large_input",
        "concurrent",
        "slow_sse",
    ] {
        assert!(rendered.contains(workload), "missing workload {workload}");
    }
    for phase in [
        "receipt_and_parsing",
        "upstream_first_response",
        "stream_relay",
    ] {
        assert!(rendered.contains(phase), "missing phase {phase}");
    }
    assert!(rendered.contains("not_measured"));

    // No payload text, no synthetic markers, no credential labels in the report.
    assert!(!rendered.contains("SYNTH-FINDING"));
    assert!(!rendered.contains("synthetic filler"));
    assert!(!rendered.contains("synthetic-event"));
    Markers::standard()
        .scan_str("perf report", &rendered)
        .unwrap();
    for label in [
        "authorization",
        "api_key",
        "api-key",
        "bearer",
        "password",
        "secret",
    ] {
        assert!(
            !rendered.to_ascii_lowercase().contains(label),
            "label {label} in report"
        );
    }
    let _ = BODY_MARKER;
}

#[test]
fn concurrent_workload_bounds_admission_and_records_overload() {
    // Memory budget admits two bodies at once; eight threads hammer it.
    let record = workloads::run_concurrent(Shape::NoFindings, 4096, 8, 20, 2);
    assert_eq!(record.iterations, 160);
    assert_eq!(record.samples.len(), 160);
    assert!(record.overload_rejections <= 160);
}

#[test]
fn percentiles_use_nearest_rank() {
    let mut s = workloads::Samples::default();
    for n in 1..=100 {
        s.push(Duration::from_nanos(n));
    }
    assert_eq!(s.percentile(50.0), 50);
    assert_eq!(s.percentile(95.0), 95);
    assert_eq!(s.percentile(99.0), 99);
    assert_eq!(workloads::Samples::default().percentile(99.0), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn alpha2_shapes_are_accepted_inspected_and_leave_no_credential_shape() {
    // Small, CI-friendly retention counts; the heavy runs are `perf_workloads --memory`.
    for shape in [
        Shape::ToolHistory,
        Shape::ToolDefs,
        Shape::Metadata,
        Shape::NodeDense,
    ] {
        let body = workloads::payload(shape, 64 * 1024);
        assert!(serde_json::from_slice::<serde_json::Value>(&body).is_ok());
        assert!(
            String::from_utf8_lossy(&body).contains("ghp_SYNTHETICREVOKED")
                || shape == Shape::NodeDense,
            "{shape:?}: plants synthetic credential shapes (node-dense plants none)"
        );
        let record = workloads::memory_phase(shape, 64 * 1024, "approved", 2).await;
        assert!(record.get("error").is_none(), "{shape:?}: {record}");
        assert_eq!(record["retained"], 2, "{shape:?}");
        assert_eq!(
            record["output_has_credential_shape"], false,
            "{shape:?}: the sealed output still holds a credential shape"
        );
        assert!(record["output_bytes"].as_u64().unwrap_or(0) > 0);
        let rendered = record.to_string();
        assert!(!rendered.contains("ghp_") && !rendered.contains("synthetic filler"));
    }
}
