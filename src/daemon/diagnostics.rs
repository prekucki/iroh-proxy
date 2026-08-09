//! Structured diagnostics for the serve-side iroh connection lifecycle.

use std::time::Duration;

use anyhow::Error;
use iroh::endpoint::{ConnectingError, Connection, ConnectionError};
use tracing::{Level, event, info};

use crate::proxy::{PumpStats, is_disconnect};

use super::routes::Route;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IncomingStage {
    AcceptStream,
    ConnectTarget,
    ProxyStream,
}

impl IncomingStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::AcceptStream => "accept_stream",
            Self::ConnectTarget => "connect_target",
            Self::ProxyStream => "proxy_stream",
        }
    }
}

#[derive(Debug)]
pub(super) struct IncomingFailure {
    stage: IncomingStage,
    error: Error,
}

impl IncomingFailure {
    pub(super) fn new(stage: IncomingStage, error: impl Into<Error>) -> Self {
        Self {
            stage,
            error: error.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagnosticLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug)]
pub(super) struct PathDiagnostics {
    count: usize,
    has_selected: bool,
    selected_kind: &'static str,
    selected_remote: Box<str>,
    selected_local: Box<str>,
    selected_rtt_ms: Option<u64>,
}

fn path_diagnostics(conn: &Connection) -> PathDiagnostics {
    let paths = conn.paths();
    let count = paths.len();
    let selected = paths.iter().find(|path| path.is_selected());
    match selected {
        Some(path) => PathDiagnostics {
            count,
            has_selected: true,
            selected_kind: if path.is_relay() {
                "relay"
            } else if path.is_ip() {
                "direct"
            } else {
                "unknown"
            },
            selected_remote: format!("{:?}", path.remote_addr()).into(),
            selected_local: format!("{:?}", path.local_addr()).into(),
            selected_rtt_ms: Some(path.rtt().as_millis().min(u64::MAX as u128) as u64),
        },
        None => PathDiagnostics {
            count,
            has_selected: false,
            selected_kind: "none",
            selected_remote: "<none>".into(),
            selected_local: "<none>".into(),
            selected_rtt_ms: None,
        },
    }
}

fn connection_error_in_chain(error: &Error) -> Option<&ConnectionError> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ConnectionError>())
}

fn io_error_in_chain(error: &Error) -> Option<&std::io::Error> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<std::io::Error>())
}

fn connection_error_level(reason: &ConnectionError) -> DiagnosticLevel {
    match reason {
        ConnectionError::LocallyClosed => DiagnosticLevel::Info,
        ConnectionError::ApplicationClosed(close) if close.error_code == 0u32.into() => {
            DiagnosticLevel::Info
        }
        ConnectionError::VersionMismatch | ConnectionError::CidsExhausted => DiagnosticLevel::Error,
        ConnectionError::TransportError(_)
        | ConnectionError::ConnectionClosed(_)
        | ConnectionError::ApplicationClosed(_)
        | ConnectionError::Reset
        | ConnectionError::TimedOut => DiagnosticLevel::Warn,
    }
}

fn connecting_error_level(error: &ConnectingError) -> DiagnosticLevel {
    match error {
        ConnectingError::ConnectionError { source, .. } => connection_error_level(source),
        ConnectingError::InternalConsistencyError { .. } => DiagnosticLevel::Error,
        ConnectingError::HandshakeFailure { .. } | ConnectingError::LocallyRejected { .. } => {
            DiagnosticLevel::Warn
        }
        _ => DiagnosticLevel::Warn,
    }
}

fn incoming_failure_level(
    failure: &IncomingFailure,
    close_reason: Option<&ConnectionError>,
) -> DiagnosticLevel {
    match failure.stage {
        IncomingStage::ConnectTarget => DiagnosticLevel::Error,
        IncomingStage::AcceptStream => connection_error_in_chain(&failure.error)
            .or(close_reason)
            .map(connection_error_level)
            .unwrap_or(DiagnosticLevel::Warn),
        IncomingStage::ProxyStream => {
            if let Some(reason) = connection_error_in_chain(&failure.error) {
                return connection_error_level(reason);
            }
            if io_error_in_chain(&failure.error).is_some_and(is_disconnect) {
                return close_reason
                    .map(connection_error_level)
                    .unwrap_or(DiagnosticLevel::Info);
            }
            DiagnosticLevel::Error
        }
    }
}

fn paths_at_failure<'a>(
    current: &'a PathDiagnostics,
    initial: &'a PathDiagnostics,
) -> (&'a PathDiagnostics, &'static str) {
    if !current.has_selected && initial.has_selected {
        (initial, "handshake_fallback")
    } else {
        (current, "current")
    }
}

pub(super) fn log_handshake_failure(
    transport_peer_addr: &str,
    transport_local_addr: &str,
    available_services: &str,
    error: ConnectingError,
) {
    let level = connecting_error_level(&error);
    let error_chain = format!("{:#}", Error::new(error));

    macro_rules! emit_handshake_failure {
        ($level:expr) => {
            event!(
                $level,
                stage = "handshake",
                peer_addr = %transport_peer_addr,
                local_addr = %transport_local_addr,
                available_services = %available_services,
                error = %error_chain,
                "incoming connection handshake failed"
            );
        };
    }

    match level {
        DiagnosticLevel::Info => {
            emit_handshake_failure!(Level::INFO);
        }
        DiagnosticLevel::Warn => {
            emit_handshake_failure!(Level::WARN);
        }
        DiagnosticLevel::Error => {
            emit_handshake_failure!(Level::ERROR);
        }
    }
}

pub(super) fn log_connection_accepted(conn: &Connection, route: &Route) -> PathDiagnostics {
    let paths = path_diagnostics(conn);
    info!(
        stage = "handshake",
        connection_id = conn.stable_id(),
        peer = %conn.remote_id(),
        service = %route.name,
        target = %route.target,
        alpn = %String::from_utf8_lossy(conn.alpn()),
        path_count = paths.count,
        path_snapshot = "handshake",
        selected_path_kind = paths.selected_kind,
        selected_path_remote = %paths.selected_remote,
        selected_path_local = %paths.selected_local,
        selected_path_rtt_ms = ?paths.selected_rtt_ms,
        "accepted incoming connection"
    );
    paths
}

pub(super) fn log_established_failure(
    conn: &Connection,
    route: &Route,
    initial_paths: &PathDiagnostics,
    connection_age: Duration,
    failure: IncomingFailure,
) {
    let peer = conn.remote_id();
    let connection_id = conn.stable_id();
    let alpn = String::from_utf8_lossy(conn.alpn());
    let current_paths = path_diagnostics(conn);
    let (paths, path_snapshot) = paths_at_failure(&current_paths, initial_paths);
    let close_reason = conn.close_reason();
    let close_reason_text = close_reason
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| "<connection still open>".to_string());
    let stats = conn.stats();
    let connection_age_ms = connection_age.as_millis().min(u64::MAX as u128) as u64;
    let error_chain = format!("{:#}", failure.error);
    let level = incoming_failure_level(&failure, close_reason.as_ref());

    macro_rules! emit_failure {
        ($level:expr) => {
            event!(
                $level,
                stage = failure.stage.as_str(),
                connection_id,
                connection_age_ms,
                peer = %peer,
                service = %route.name,
                target = %route.target,
                alpn = %alpn,
                path_count = current_paths.count,
                path_snapshot,
                selected_path_kind = paths.selected_kind,
                selected_path_remote = %paths.selected_remote,
                selected_path_local = %paths.selected_local,
                selected_path_rtt_ms = ?paths.selected_rtt_ms,
                close_reason = %close_reason_text,
                quic_lost_packets = stats.lost_packets,
                quic_lost_bytes = stats.lost_bytes,
                error = %error_chain,
                "incoming connection failed"
            );
        };
    }

    match level {
        DiagnosticLevel::Info => {
            emit_failure!(Level::INFO);
        }
        DiagnosticLevel::Warn => {
            emit_failure!(Level::WARN);
        }
        DiagnosticLevel::Error => {
            emit_failure!(Level::ERROR);
        }
    }
}

pub(super) fn log_connection_finished(
    conn: &Connection,
    route: &Route,
    initial_paths: &PathDiagnostics,
    connection_age: Duration,
    pump_stats: PumpStats,
    close_reason: ConnectionError,
) {
    let peer = conn.remote_id();
    let connection_id = conn.stable_id();
    let current_paths = path_diagnostics(conn);
    let (paths, path_snapshot) = paths_at_failure(&current_paths, initial_paths);
    let stats = conn.stats();
    let connection_age_ms = connection_age.as_millis().min(u64::MAX as u128) as u64;
    let level = connection_error_level(&close_reason);

    macro_rules! emit_finished {
        ($level:expr) => {
            event!(
                $level,
                stage = "closed",
                connection_id,
                connection_age_ms,
                peer = %peer,
                service = %route.name,
                target_to_peer_bytes = pump_stats.up_bytes,
                peer_to_target_bytes = pump_stats.down_bytes,
                close_reason = %close_reason,
                path_count = current_paths.count,
                path_snapshot,
                selected_path_kind = paths.selected_kind,
                selected_path_remote = %paths.selected_remote,
                selected_path_local = %paths.selected_local,
                selected_path_rtt_ms = ?paths.selected_rtt_ms,
                quic_lost_packets = stats.lost_packets,
                quic_lost_bytes = stats.lost_bytes,
                "incoming connection finished"
            );
        };
    }

    match level {
        DiagnosticLevel::Info => {
            emit_finished!(Level::INFO);
        }
        DiagnosticLevel::Warn => {
            emit_finished!(Level::WARN);
        }
        DiagnosticLevel::Error => {
            emit_finished!(Level::ERROR);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_error_levels_distinguish_clean_close_from_transport_failure() {
        assert_eq!(
            connection_error_level(&ConnectionError::LocallyClosed),
            DiagnosticLevel::Info
        );
        assert_eq!(
            connection_error_level(&ConnectionError::TimedOut),
            DiagnosticLevel::Warn
        );
        assert_eq!(
            connection_error_level(&ConnectionError::Reset),
            DiagnosticLevel::Warn
        );
        assert_eq!(
            connection_error_level(&ConnectionError::CidsExhausted),
            DiagnosticLevel::Error
        );
    }

    #[test]
    fn failure_level_reads_connection_reason_from_error_chain() {
        let io_error: std::io::Error =
            iroh::endpoint::ReadError::ConnectionLost(ConnectionError::TimedOut).into();
        let failure = IncomingFailure::new(
            IncomingStage::ProxyStream,
            Error::new(io_error).context("iroh->local copy failed"),
        );
        assert_eq!(
            incoming_failure_level(&failure, None),
            DiagnosticLevel::Warn
        );
        let error_chain = format!("{:#}", failure.error);
        assert!(error_chain.contains("connection lost"));
        assert!(error_chain.contains("timed out"));
    }

    #[test]
    fn local_target_failure_is_always_an_error() {
        let failure = IncomingFailure::new(
            IncomingStage::ConnectTarget,
            anyhow::anyhow!("connection refused"),
        );
        assert_eq!(
            incoming_failure_level(&failure, Some(&ConnectionError::LocallyClosed)),
            DiagnosticLevel::Error
        );
    }

    #[test]
    fn expected_stream_disconnect_is_not_escalated() {
        let failure = IncomingFailure::new(
            IncomingStage::ProxyStream,
            Error::new(std::io::Error::from(std::io::ErrorKind::BrokenPipe)).context("copy failed"),
        );
        assert_eq!(
            incoming_failure_level(&failure, None),
            DiagnosticLevel::Info
        );
    }

    #[test]
    fn unexpected_stream_error_wins_over_clean_close_reason() {
        let failure = IncomingFailure::new(
            IncomingStage::ProxyStream,
            Error::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
                .context("copy failed"),
        );
        assert_eq!(
            incoming_failure_level(&failure, Some(&ConnectionError::LocallyClosed)),
            DiagnosticLevel::Error
        );
    }

    #[test]
    fn missing_failure_path_falls_back_to_handshake_snapshot() {
        let initial = PathDiagnostics {
            count: 1,
            has_selected: true,
            selected_kind: "relay",
            selected_remote: "relay".into(),
            selected_local: "relay".into(),
            selected_rtt_ms: Some(125),
        };
        let current = PathDiagnostics {
            count: 0,
            has_selected: false,
            selected_kind: "none",
            selected_remote: "<none>".into(),
            selected_local: "<none>".into(),
            selected_rtt_ms: None,
        };

        let (selected, source) = paths_at_failure(&current, &initial);
        assert_eq!(source, "handshake_fallback");
        assert_eq!(selected.count, 1);
        assert_eq!(selected.selected_kind, "relay");
    }

    #[test]
    fn missing_selected_path_falls_back_even_when_other_paths_remain() {
        let initial = PathDiagnostics {
            count: 1,
            has_selected: true,
            selected_kind: "relay",
            selected_remote: "relay".into(),
            selected_local: "relay".into(),
            selected_rtt_ms: Some(125),
        };
        let current = PathDiagnostics {
            count: 2,
            has_selected: false,
            selected_kind: "none",
            selected_remote: "<none>".into(),
            selected_local: "<none>".into(),
            selected_rtt_ms: None,
        };

        let (selected, source) = paths_at_failure(&current, &initial);
        assert_eq!(source, "handshake_fallback");
        assert_eq!(selected.selected_kind, "relay");
    }
}
