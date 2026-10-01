// 覆盖:TianQuan TEST-SPEC AE-L1-156 / ISSUE-IRONCLAW-022。
use ironclaw_reborn_config::HostHttpTimeouts;

#[test]
fn defaults_keep_existing_bounded_transport_and_mcp_limits() {
    let config = HostHttpTimeouts::parse(None, None).expect("defaults");
    assert_eq!(config.host_timeout_ms, 30_000);
    assert_eq!(config.mcp_timeout_ms, 60_000);
    assert_eq!(config.effective_mcp_timeout_ms(), 30_000);
}

#[test]
fn explicit_long_request_is_still_bounded_by_host_ceiling() {
    let config =
        HostHttpTimeouts::parse(Some("360000"), Some("300000")).expect("bounded long call");
    assert_eq!(config.effective_mcp_timeout_ms(), 300_000);
    let config = HostHttpTimeouts::parse(Some("50000"), Some("300000")).expect("ceiling");
    assert_eq!(config.effective_mcp_timeout_ms(), 50_000);
}

#[test]
fn invalid_budgets_fail_closed() {
    for value in ["", "0", "900001", "-1", "forever", "4294967296"] {
        assert!(HostHttpTimeouts::parse(Some(value), None).is_err());
        assert!(HostHttpTimeouts::parse(None, Some(value)).is_err());
    }
}

#[test]
fn boundary_values_are_finite_and_accepted() {
    for value in ["1", "900000"] {
        assert!(HostHttpTimeouts::parse(Some(value), Some(value)).is_ok());
    }
}
