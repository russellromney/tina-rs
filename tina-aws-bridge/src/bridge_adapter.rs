//! Adapters from per-service AWS bridge pressure reports to the shared
//! [`BridgePressure`] vocabulary.
//!
//! Each AWS service exposes its own typed pressure report with the same
//! shape (capacity, available, leased, waiters, full/closed/timeout
//! counters, late-result count, high-water). The shared
//! [`BridgePressure`] is the cross-bridge view dashboards consume.
//!
//! These adapters use the typed [`BridgePressure::measured`]
//! constructor; they cannot forge a name or lie about installed
//! capacity because the destination struct holds private fields. The
//! surface name is hard-coded per service so a buggy caller cannot
//! re-name an existing dashboard surface.

use tina::Shard;
use tina_runtime::bridge::{BridgeCloser, BridgeInstall, BridgePressure};

use crate::dynamodb_metrics::{DynamoMetricsHandle, DynamoPressureReport};
use crate::dynamodb_worker::{DynamoCloser, InstalledDynamoBridge};
use crate::metrics::{S3MetricsHandle, S3PressureReport};
use crate::secrets_metrics::{SecretsMetricsHandle, SecretsPressureReport};
use crate::secrets_worker::{InstalledSecretsBridge, SecretsCloser};
use crate::sns_metrics::{SnsMetricsHandle, SnsPressureReport};
use crate::sns_worker::{InstalledSnsBridge, SnsCloser};
use crate::sqs_metrics::{SqsMetricsHandle, SqsPressureReport};
use crate::sqs_worker::{InstalledSqsBridge, SqsCloser};
use crate::worker::{InstalledS3Bridge, S3Closer};

/// Stable surface name for the S3 bridge pressure surface.
pub const S3_BRIDGE_SURFACE: &str = "aws.s3";
/// Stable surface name for the SQS bridge pressure surface.
pub const SQS_BRIDGE_SURFACE: &str = "aws.sqs";
/// Stable surface name for the DynamoDB bridge pressure surface.
pub const DYNAMODB_BRIDGE_SURFACE: &str = "aws.dynamodb";
/// Stable surface name for the SNS bridge pressure surface.
pub const SNS_BRIDGE_SURFACE: &str = "aws.sns";
/// Stable surface name for the Secrets Manager bridge pressure surface.
pub const SECRETS_BRIDGE_SURFACE: &str = "aws.secrets_manager";

impl From<S3PressureReport> for BridgePressure {
    fn from(r: S3PressureReport) -> Self {
        // worker_terminal_count: the bridge only counts late terminal
        // completions (caller had already given up). Total
        // worker-terminal events (responses + sdk_errors) are not
        // separately exposed at this layer. Conservatively use the
        // late-result count so this field is never an over-claim.
        BridgePressure::measured(
            S3_BRIDGE_SURFACE,
            r.capacity,
            r.leased,
            r.high_water,
            r.full_count,
            r.timeout_count,
            r.closed_count,
            r.late_result_count,
            r.late_result_count,
        )
        .expect("S3 bridge surface name is a valid bridge name")
    }
}

impl From<SqsPressureReport> for BridgePressure {
    fn from(r: SqsPressureReport) -> Self {
        BridgePressure::measured(
            SQS_BRIDGE_SURFACE,
            r.capacity,
            r.leased,
            r.high_water,
            r.full_count,
            r.timeout_count,
            r.closed_count,
            r.late_result_count,
            r.late_result_count,
        )
        .expect("SQS bridge surface name is a valid bridge name")
    }
}

impl From<DynamoPressureReport> for BridgePressure {
    fn from(r: DynamoPressureReport) -> Self {
        BridgePressure::measured(
            DYNAMODB_BRIDGE_SURFACE,
            r.capacity,
            r.leased,
            r.high_water,
            r.full_count,
            r.timeout_count,
            r.closed_count,
            r.late_result_count,
            r.late_result_count,
        )
        .expect("DynamoDB bridge surface name is a valid bridge name")
    }
}

impl From<SnsPressureReport> for BridgePressure {
    fn from(r: SnsPressureReport) -> Self {
        BridgePressure::measured(
            SNS_BRIDGE_SURFACE,
            r.capacity,
            r.leased,
            r.high_water,
            r.full_count,
            r.timeout_count,
            r.closed_count,
            r.late_result_count,
            r.late_result_count,
        )
        .expect("SNS bridge surface name is a valid bridge name")
    }
}

impl From<SecretsPressureReport> for BridgePressure {
    fn from(r: SecretsPressureReport) -> Self {
        BridgePressure::measured(
            SECRETS_BRIDGE_SURFACE,
            r.capacity,
            r.leased,
            r.high_water,
            r.full_count,
            r.timeout_count,
            r.closed_count,
            r.late_result_count,
            r.late_result_count,
        )
        .expect("Secrets Manager bridge surface name is a valid bridge name")
    }
}

// -------------------------------------------------------------------
// BridgeCloser + BridgeInstall vocabulary impls.
//
// The shared traits are minimal: BridgeCloser names `close()` /
// `is_closed()`, BridgeInstall names the closer + metrics handle. The
// per-bridge drain reports stay typed (each has its own
// `XxxDrainReport`); cross-bridge tools that want a uniform view can
// build one through these traits.
// -------------------------------------------------------------------

macro_rules! impl_bridge_closer {
    ($t:ty) => {
        impl BridgeCloser for $t {
            fn close(&self) {
                <$t>::close(self)
            }
            fn is_closed(&self) -> bool {
                <$t>::is_closed(self)
            }
        }
    };
}

impl_bridge_closer!(S3Closer);
impl_bridge_closer!(SqsCloser);
impl_bridge_closer!(SnsCloser);
impl_bridge_closer!(DynamoCloser);
impl_bridge_closer!(SecretsCloser);

impl<S: Shard + 'static> BridgeInstall for InstalledS3Bridge<S> {
    type Closer = S3Closer;
    type Metrics = S3MetricsHandle;
    fn closer(&self) -> &S3Closer {
        &self.closer
    }
    fn metrics(&self) -> &S3MetricsHandle {
        &self.metrics
    }
}

impl<S: Shard + 'static> BridgeInstall for InstalledSqsBridge<S> {
    type Closer = SqsCloser;
    type Metrics = SqsMetricsHandle;
    fn closer(&self) -> &SqsCloser {
        &self.closer
    }
    fn metrics(&self) -> &SqsMetricsHandle {
        &self.metrics
    }
}

impl<S: Shard + 'static> BridgeInstall for InstalledSnsBridge<S> {
    type Closer = SnsCloser;
    type Metrics = SnsMetricsHandle;
    fn closer(&self) -> &SnsCloser {
        &self.closer
    }
    fn metrics(&self) -> &SnsMetricsHandle {
        &self.metrics
    }
}

impl<S: Shard + 'static> BridgeInstall for InstalledDynamoBridge<S> {
    type Closer = DynamoCloser;
    type Metrics = DynamoMetricsHandle;
    fn closer(&self) -> &DynamoCloser {
        &self.closer
    }
    fn metrics(&self) -> &DynamoMetricsHandle {
        &self.metrics
    }
}

impl<S: Shard + 'static> BridgeInstall for InstalledSecretsBridge<S> {
    type Closer = SecretsCloser;
    type Metrics = SecretsMetricsHandle;
    fn closer(&self) -> &SecretsCloser {
        &self.closer
    }
    fn metrics(&self) -> &SecretsMetricsHandle {
        &self.metrics
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s3_pressure_round_trips_with_installed_capacity() {
        let r = S3PressureReport {
            capacity: 32,
            available: 27,
            leased: 5,
            waiters: 0,
            max_waiters: 0,
            full_count: 3,
            closed_count: 1,
            timeout_count: 2,
            late_result_count: 4,
            high_water: 17,
        };
        let bp: BridgePressure = r.into();
        assert_eq!(bp.name(), S3_BRIDGE_SURFACE);
        assert_eq!(bp.capacity(), 32);
        assert_eq!(bp.current(), 5);
        assert_eq!(bp.high_water(), 17);
        assert_eq!(bp.full_count(), 3);
        assert_eq!(bp.timeout_count(), 2);
        assert_eq!(bp.closed_count(), 1);
        assert_eq!(bp.late_result_count(), 4);
        assert_eq!(bp.worker_terminal_count(), 4);
    }

    #[test]
    fn sqs_pressure_round_trips() {
        let r = SqsPressureReport {
            capacity: 8,
            available: 8,
            leased: 0,
            waiters: 0,
            max_waiters: 0,
            full_count: 0,
            closed_count: 0,
            timeout_count: 0,
            late_result_count: 0,
            high_water: 0,
        };
        let bp: BridgePressure = r.into();
        assert_eq!(bp.name(), SQS_BRIDGE_SURFACE);
        assert_eq!(bp.capacity(), 8);
    }

    #[test]
    fn dynamodb_pressure_round_trips() {
        let r = DynamoPressureReport {
            capacity: 16,
            available: 14,
            leased: 2,
            waiters: 0,
            max_waiters: 0,
            full_count: 0,
            closed_count: 0,
            timeout_count: 0,
            late_result_count: 0,
            high_water: 3,
        };
        let bp: BridgePressure = r.into();
        assert_eq!(bp.name(), DYNAMODB_BRIDGE_SURFACE);
        assert_eq!(bp.capacity(), 16);
    }

    #[test]
    fn sns_pressure_round_trips() {
        let r = SnsPressureReport {
            capacity: 4,
            available: 4,
            leased: 0,
            waiters: 0,
            max_waiters: 0,
            full_count: 0,
            closed_count: 0,
            timeout_count: 0,
            late_result_count: 0,
            high_water: 0,
        };
        let bp: BridgePressure = r.into();
        assert_eq!(bp.name(), SNS_BRIDGE_SURFACE);
    }

    #[test]
    fn secrets_pressure_round_trips() {
        let r = SecretsPressureReport {
            capacity: 2,
            available: 2,
            leased: 0,
            waiters: 0,
            max_waiters: 0,
            full_count: 0,
            closed_count: 0,
            timeout_count: 0,
            late_result_count: 0,
            high_water: 0,
        };
        let bp: BridgePressure = r.into();
        assert_eq!(bp.name(), SECRETS_BRIDGE_SURFACE);
    }

    // Static type-level proof that BridgeCloser is implemented for
    // every AWS closer. If a future refactor breaks the impl, this
    // stops compiling.
    fn _assert_bridge_closer_impls() {
        fn assert_impl<T: BridgeCloser>() {}
        assert_impl::<S3Closer>();
        assert_impl::<SqsCloser>();
        assert_impl::<SnsCloser>();
        assert_impl::<DynamoCloser>();
        assert_impl::<SecretsCloser>();
    }
}
