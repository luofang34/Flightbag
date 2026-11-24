use std::io::Cursor;

use super::*;

struct FixedTransport {
    status: u16,
    validators: UpstreamValidators,
}

impl Transport for FixedTransport {
    fn get_blocking(
        &self,
        _url: &str,
        range: Option<&str>,
    ) -> Result<TransportResponse, FetchError> {
        assert_eq!(range, Some("bytes=0-0"));
        Ok(TransportResponse {
            status: self.status,
            body: Box::new(Cursor::new(Vec::new())),
            validators: self.validators.clone(),
        })
    }
}

fn posted() -> UpstreamValidators {
    UpstreamValidators {
        last_modified: Some("Wed, 28 Oct 2026 18:21:52 GMT".to_owned()),
        etag: Some("W/\"259273723-1793211712085\"".to_owned()),
        size_bytes: Some(259_273_723),
    }
}

#[test]
fn probe_reports_validators_of_a_served_archive() {
    let transport = FixedTransport {
        status: 206,
        validators: posted(),
    };
    let probe = probe_upstream_archive(&transport, "https://example.invalid/a.zip").unwrap();
    assert!(probe.published);
    assert_eq!(probe.validators, posted());
}

#[test]
fn probe_treats_not_found_as_unpublished_without_validators() {
    for status in [404, 410] {
        let transport = FixedTransport {
            status,
            validators: posted(),
        };
        let probe = probe_upstream_archive(&transport, "https://example.invalid/a.zip").unwrap();
        assert!(!probe.published);
        assert_eq!(probe.validators, UpstreamValidators::default());
    }
}

#[test]
fn probe_surfaces_an_outage_as_an_error_not_as_unpublished() {
    let transport = FixedTransport {
        status: 503,
        validators: posted(),
    };
    let err = probe_upstream_archive(&transport, "https://example.invalid/a.zip").unwrap_err();
    assert!(matches!(err, FetchError::Status { status: 503, .. }));
}

#[test]
fn total_size_comes_from_content_range_or_whole_body_length() {
    assert_eq!(
        total_size_bytes(206, Some("bytes 0-0/259273723"), Some("1")),
        Some(259_273_723)
    );
    assert_eq!(
        total_size_bytes(200, None, Some("9116096")),
        Some(9_116_096)
    );
    assert_eq!(total_size_bytes(206, Some("bytes 0-0/*"), Some("1")), None);
    assert_eq!(total_size_bytes(404, None, Some("397")), None);
}
