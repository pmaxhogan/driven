//! Pure, transport-result -> [`ProbeOutcome`] classification (DESIGN
//! s5.8.1 failure modes).
//!
//! Split out from [`crate::ReqwestBackend`] so the status/error mapping is
//! exercised by offline unit tests that need no real socket: every test in
//! this module constructs a `StatusCode` or a synthetic transport result and
//! asserts the resulting [`ProbeOutcome`]. The backend's probe methods are
//! thin wrappers that perform the actual I/O and then funnel through these
//! functions, so the I/O-bearing paths carry no untested branching.
//!
//! The mapping table (DESIGN s5.8.1 / the M4 probe spec):
//!
//! | Transport result                          | Captive probe              | Service probe              |
//! |-------------------------------------------|----------------------------|----------------------------|
//! | bare `204 No Content`, empty body         | [`ProbeOutcome::Ok`]       | n/a (captive only)         |
//! | any other 2xx / 3xx / 4xx (incl. 401)     | [`ProbeOutcome::CaptivePortal`] | [`ProbeOutcome::Ok`]  |
//! | `5xx`                                     | [`ProbeOutcome::CaptivePortal`] (captive sees any non-204 as a portal) | [`ProbeOutcome::ServiceError`] |
//! | DNS resolution failure (NXDOMAIN / empty) | [`ProbeOutcome::DnsFailed`] | [`ProbeOutcome::DnsFailed`] |
//! | connect refused / reset / connect timeout | [`ProbeOutcome::NetworkError`] | [`ProbeOutcome::NetworkError`] |
//! | total / idle timeout                      | [`ProbeOutcome::NetworkError`] | [`ProbeOutcome::NetworkError`] |
//! | redirect (portal 30x captured, not followed) | [`ProbeOutcome::CaptivePortal`] | n/a (services follow none too -> treated as service answer) |

use driven_core::network::ProbeOutcome;
use reqwest::StatusCode;

/// Classifies a transport-level `reqwest::Error` (no HTTP response was
/// produced) into a [`ProbeOutcome`] for a *service* probe.
///
/// DNS resolution is performed up-front by the backend via `hickory` and
/// surfaces [`ProbeOutcome::DnsFailed`] before any `reqwest` call, so a
/// `reqwest::Error` reaching here is a connect/timeout/body failure, never a
/// name-resolution failure: it maps to [`ProbeOutcome::NetworkError`] (which
/// counts toward the pool-teardown threshold, DESIGN s5.8.5).
///
/// A `reqwest::Error` that nonetheless carries an HTTP status (only possible
/// when the caller used `error_for_status`, which the probes do NOT) is
/// re-routed through [`classify_service_status`] so the mapping stays single-
/// sourced; the probes read the status off the `Response` directly instead.
pub fn classify_service_transport_error(err: &reqwest::Error) -> ProbeOutcome {
    if let Some(status) = err.status() {
        return classify_service_status(status);
    }
    // is_connect / is_timeout / is_request / is_body all describe a network-
    // level failure with no usable HTTP answer: the service may be fine but
    // the connection is not, so this counts as a network-level error
    // (pool-teardown-eligible), NOT a service error.
    ProbeOutcome::NetworkError
}

/// Classifies a *service* probe's HTTP status (DESIGN s5.8.2 probe 3).
///
/// Per the M4 spec any HTTP response - including `401 Unauthorized` from an
/// unauthenticated Drive `about` probe - proves the SERVICE is reachable, so
/// only a `5xx` server error counts as the service being down. `2xx`/`3xx`/
/// `4xx` all mean "service reachable" -> [`ProbeOutcome::Ok`].
pub fn classify_service_status(status: StatusCode) -> ProbeOutcome {
    if status.is_server_error() {
        ProbeOutcome::ServiceError
    } else {
        ProbeOutcome::Ok
    }
}

/// Classifies the captive-portal `generate_204` probe's HTTP status (DESIGN
/// s5.8.1 / s5.8.2 probe 2).
///
/// A bare `204 No Content` with an empty body is the only "clean link"
/// answer. ANY other status - a `200` with a portal login page, a `30x`
/// redirect to a sign-in URL, a `403`, even a `5xx` from an intercepting
/// proxy - means something is sitting between us and the Internet:
/// [`ProbeOutcome::CaptivePortal`]. The body-emptiness check is layered on
/// top of this by [`classify_captive_204`].
pub fn classify_captive_status(status: StatusCode) -> ProbeOutcome {
    if status == StatusCode::NO_CONTENT {
        ProbeOutcome::Ok
    } else {
        ProbeOutcome::CaptivePortal
    }
}

/// Final captive-portal verdict from the observed status AND whether the body
/// was empty (DESIGN s5.8.1: "204 with empty body").
///
/// `gstatic.com/generate_204` returns `204` with a zero-length body on a
/// clean link. A `204` carrying a body (some portals fake the status but
/// inject HTML) is treated as a captive portal. Any non-204 is already a
/// portal per [`classify_captive_status`].
pub fn classify_captive_204(status: StatusCode, body_is_empty: bool) -> ProbeOutcome {
    match classify_captive_status(status) {
        ProbeOutcome::Ok if body_is_empty => ProbeOutcome::Ok,
        ProbeOutcome::Ok => ProbeOutcome::CaptivePortal,
        other => other,
    }
}

/// Classifies a transport-level `reqwest::Error` from the captive probe.
///
/// The redirect policy on the captive client is `none()`, so a portal's
/// `30x` is returned as a [`reqwest::Response`] (handled via
/// [`classify_captive_204`]) rather than raised as an `is_redirect` error -
/// but if `reqwest` ever does surface a redirect error we still read it as a
/// portal. DNS failures are caught up-front by the backend's `hickory`
/// resolve, so a transport error here is a connect/timeout failure ->
/// [`ProbeOutcome::NetworkError`] (link-local-only "no Internet" while the OS
/// claims connectivity; the prober folds this to `NoInternet`).
pub fn classify_captive_transport_error(err: &reqwest::Error) -> ProbeOutcome {
    if err.is_redirect() {
        return ProbeOutcome::CaptivePortal;
    }
    if let Some(status) = err.status() {
        return classify_captive_status(status);
    }
    ProbeOutcome::NetworkError
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- service status mapping (DESIGN s5.8.2 probe 3) ---

    #[test]
    fn service_2xx_is_reachable() {
        assert_eq!(classify_service_status(StatusCode::OK), ProbeOutcome::Ok);
        assert_eq!(
            classify_service_status(StatusCode::NO_CONTENT),
            ProbeOutcome::Ok
        );
    }

    #[test]
    fn service_401_means_reachable_not_down() {
        // The Drive `about` probe is unauthenticated; a 401 still proves the
        // SERVICE is up (M4 spec / DESIGN s5.8.2).
        assert_eq!(
            classify_service_status(StatusCode::UNAUTHORIZED),
            ProbeOutcome::Ok
        );
        assert_eq!(
            classify_service_status(StatusCode::FORBIDDEN),
            ProbeOutcome::Ok
        );
        assert_eq!(
            classify_service_status(StatusCode::NOT_FOUND),
            ProbeOutcome::Ok
        );
        assert_eq!(
            classify_service_status(StatusCode::TOO_MANY_REQUESTS),
            ProbeOutcome::Ok
        );
    }

    #[test]
    fn service_3xx_means_reachable() {
        assert_eq!(
            classify_service_status(StatusCode::MOVED_PERMANENTLY),
            ProbeOutcome::Ok
        );
    }

    #[test]
    fn service_5xx_is_service_error() {
        for code in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
        ] {
            assert_eq!(classify_service_status(code), ProbeOutcome::ServiceError);
        }
    }

    // --- captive status mapping (DESIGN s5.8.1 / s5.8.2 probe 2) ---

    #[test]
    fn captive_bare_204_empty_body_is_ok() {
        assert_eq!(
            classify_captive_204(StatusCode::NO_CONTENT, true),
            ProbeOutcome::Ok
        );
    }

    #[test]
    fn captive_204_with_body_is_portal() {
        // Some portals fake the 204 status but inject a body.
        assert_eq!(
            classify_captive_204(StatusCode::NO_CONTENT, false),
            ProbeOutcome::CaptivePortal
        );
    }

    #[test]
    fn captive_200_is_portal() {
        // A login splash page returns 200, not 204.
        assert_eq!(
            classify_captive_204(StatusCode::OK, true),
            ProbeOutcome::CaptivePortal
        );
        assert_eq!(
            classify_captive_status(StatusCode::OK),
            ProbeOutcome::CaptivePortal
        );
    }

    #[test]
    fn captive_redirect_is_portal() {
        // A 302 to a sign-in URL: classified off the status (redirect policy
        // is none(), so it arrives as a response, not a followed redirect).
        assert_eq!(
            classify_captive_status(StatusCode::FOUND),
            ProbeOutcome::CaptivePortal
        );
        assert_eq!(
            classify_captive_204(StatusCode::TEMPORARY_REDIRECT, true),
            ProbeOutcome::CaptivePortal
        );
    }

    #[test]
    fn captive_5xx_is_portal_not_service_error() {
        // An intercepting proxy 5xx is still "something between us and the
        // Internet" for the captive probe.
        assert_eq!(
            classify_captive_204(StatusCode::BAD_GATEWAY, true),
            ProbeOutcome::CaptivePortal
        );
    }
}
