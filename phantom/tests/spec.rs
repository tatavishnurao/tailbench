//! Spec parser tests: what `--service-ms`, `--lognormal` and `--stall` accept,
//! and that a script with an impossible combination is refused.
//!
//! Durations must be positive; a stall may start at 0 (the first request's
//! arrival) because that is a real moment, not an empty duration.

use phantom::{Script, ServiceSpec, Stall, parse_lognormal, parse_service_ms, parse_stall};

#[test]
fn service_ms_accepts_a_positive_integer() {
    assert_eq!(parse_service_ms("20"), Ok(ServiceSpec::Fixed { ms: 20 }));
    assert_eq!(parse_service_ms("1"), Ok(ServiceSpec::Fixed { ms: 1 }));
}

#[test]
fn service_ms_rejects_zero_negative_and_malformed() {
    for bad in ["0", "-5", "", " ", "abc", "20ms", "1.5", "18446744073709551616"] {
        let err = parse_service_ms(bad).expect_err(bad);
        assert!(!err.to_string().is_empty(), "{bad:?}: empty error message");
    }
}

#[test]
fn lognormal_accepts_median_and_sigma() {
    assert_eq!(
        parse_lognormal("20:0.5"),
        Ok(ServiceSpec::Lognormal {
            median_ms: 20.0,
            sigma: 0.5
        })
    );
    assert_eq!(
        parse_lognormal("0.25:2"),
        Ok(ServiceSpec::Lognormal {
            median_ms: 0.25,
            sigma: 2.0
        })
    );
}

#[test]
fn lognormal_rejects_zero_negative_non_finite_and_malformed() {
    for bad in [
        "0:0.5", "-1:0.5", "20:0", "20:-0.1", "nan:0.5", "20:NaN", "inf:0.5", "20:inf", "20", "20:", ":0.5",
        "20:0.5:1", "a:b", "", "20;0.5",
    ] {
        let err = parse_lognormal(bad).expect_err(bad);
        assert!(
            err.to_string().contains(bad),
            "{bad:?}: message does not quote the input: {err}"
        );
    }
}

#[test]
fn stall_accepts_start_and_length() {
    assert_eq!(
        parse_stall("100:300"),
        Ok(Stall {
            start_ms: 100,
            len_ms: 300
        })
    );
    assert_eq!(parse_stall("0:5"), Ok(Stall { start_ms: 0, len_ms: 5 }));
}

#[test]
fn stall_rejects_zero_length_negative_and_malformed() {
    for bad in [
        "100:0",
        "-1:300",
        "100:-300",
        "100",
        "100:",
        ":300",
        "100:300:5",
        "a:b",
        "1.5:300",
        "",
    ] {
        let err = parse_stall(bad).expect_err(bad);
        assert!(
            err.to_string().contains(bad),
            "{bad:?}: message does not quote the input: {err}"
        );
    }
}

#[test]
fn script_rejects_overlapping_stalls() {
    let fixed = ServiceSpec::Fixed { ms: 1 };
    let a = Stall {
        start_ms: 100,
        len_ms: 300,
    };
    let inside = Stall {
        start_ms: 200,
        len_ms: 50,
    };
    let touching = Stall {
        start_ms: 400,
        len_ms: 50,
    };
    let err = Script::new(1, fixed, vec![a, inside]).expect_err("overlap");
    assert!(err.to_string().contains("overlap"), "{err}");
    // Order on the command line does not matter.
    Script::new(1, fixed, vec![inside, a]).expect_err("overlap, given in reverse");
    // A window that starts where another ends does not overlap it.
    Script::new(1, fixed, vec![a, touching]).expect("touching windows");
}

#[test]
fn script_rejects_specs_built_without_the_parser() {
    let none: Vec<Stall> = Vec::new();
    Script::new(1, ServiceSpec::Fixed { ms: 0 }, none.clone()).expect_err("fixed 0 ms");
    for (median_ms, sigma) in [
        (0.0, 0.5),
        (-1.0, 0.5),
        (20.0, 0.0),
        (f64::NAN, 0.5),
        (20.0, f64::INFINITY),
    ] {
        Script::new(1, ServiceSpec::Lognormal { median_ms, sigma }, none.clone())
            .expect_err(&format!("lognormal {median_ms}:{sigma}"));
    }
    let zero_len = Stall {
        start_ms: 10,
        len_ms: 0,
    };
    Script::new(1, ServiceSpec::Fixed { ms: 1 }, vec![zero_len]).expect_err("zero-length stall");
    // The window end must be representable in nanoseconds.
    let huge = Stall {
        start_ms: u64::MAX / 1_000_000,
        len_ms: 1_000,
    };
    Script::new(1, ServiceSpec::Fixed { ms: 1 }, vec![huge]).expect_err("window end overflows u64 ns");
}

#[test]
fn script_answers_from_its_spec() {
    let script = Script::new(
        7,
        ServiceSpec::Fixed { ms: 20 },
        vec![Stall {
            start_ms: 100,
            len_ms: 300,
        }],
    )
    .unwrap();
    assert_eq!(script.scripted_service_ns(0), 20_000_000);
    assert_eq!(script.scripted_service_ns(u64::MAX), 20_000_000);
    // The window is [100 ms, 400 ms): start inclusive, end exclusive.
    assert_eq!(script.stall_wait_ns(99_999_999), 0);
    assert_eq!(script.stall_wait_ns(100_000_000), 300_000_000);
    assert_eq!(script.stall_wait_ns(250_000_000), 150_000_000);
    assert_eq!(script.stall_wait_ns(399_999_999), 1);
    assert_eq!(script.stall_wait_ns(400_000_000), 0);
}
