//! Public message, request, response, value, row, error, and config
//! types for the Postgres bridge.

use std::time::Duration;

/// Hard cap on `mailbox_capacity` so an accidental `usize::MAX` cannot
/// quietly turn the bridge mailbox into an unbounded queue.
pub(crate) const MAX_MAILBOX_CAPACITY: usize = 1 << 20;

/// Postgres type descriptor used as a hint when binding NULL values.
///
/// `PgValue::Null` binds as an untyped null (encoded as `INT8 NULL`),
/// which Postgres usually infers from the surrounding query — but
/// not always. When you bind NULL into a positional parameter of a
/// non-INT8 column without a SQL cast, Postgres returns a
/// type-mismatch error. [`PgValue::TypedNull`] uses this enum to
/// pick the right SQLx encoder so the wire-level type oid matches
/// the column.
///
/// One variant per supported [`PgValue`] type. Feature-gated entries
/// mirror the feature-gated `PgValue` variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgType {
    /// Postgres `BOOL`.
    Bool,
    /// Postgres `INT2` / `INT4` / `INT8`. Bound as `INT8 NULL`.
    I64,
    /// Postgres `FLOAT4` / `FLOAT8`. Bound as `FLOAT8 NULL`.
    F64,
    /// Postgres `TEXT` / `VARCHAR` / `CHAR` / `BPCHAR` / `NAME` /
    /// `CITEXT`.
    Text,
    /// Postgres `BYTEA`.
    Bytes,
    /// Postgres `UUID`. Available with the `uuid` feature.
    #[cfg(feature = "uuid")]
    Uuid,
    /// Postgres `JSON` / `JSONB`. Bound as `JSONB NULL` (compatible
    /// with both Postgres column types in practice). Available with
    /// the `json` feature.
    #[cfg(feature = "json")]
    Json,
    /// Postgres `NUMERIC`. Available with the `numeric` feature.
    #[cfg(feature = "numeric")]
    Numeric,
    /// Postgres `TIMESTAMP` (without time zone). Available with the
    /// `time` feature.
    #[cfg(feature = "time")]
    Timestamp,
    /// Postgres `TIMESTAMPTZ`. Available with the `time` feature.
    #[cfg(feature = "time")]
    TimestampTz,
    /// Postgres `DATE`. Available with the `time` feature.
    #[cfg(feature = "time")]
    Date,
}

/// Postgres value at the bridge boundary.
///
/// The base variants cover the boring core of Postgres types: bool,
/// signed integers up to 64 bits, float, text, and bytes. Wider
/// types (UUID, JSON/B, NUMERIC, temporal) are gated behind cargo
/// features (`uuid`, `json`, `numeric`, `time`); enabling the
/// feature pulls the matching SQLx feature too, so the bridge can
/// encode and decode the type. Unsupported column types still fail
/// visibly with [`PgError::Decode`] rather than being silently
/// coerced.
#[derive(Debug, Clone, PartialEq)]
pub enum PgValue {
    /// Untyped SQL `NULL`. On bind, sent as `INT8 NULL` and
    /// Postgres infers the column type from the query (e.g. an
    /// `INSERT INTO t (col) VALUES ($1)` resolves through the table
    /// schema). On decode, any column's missing value lands here.
    ///
    /// **When Postgres can't infer** (a positional NULL bind into a
    /// non-INT8 column without a SQL cast or surrounding type
    /// information), use [`Self::TypedNull`] to pick the right
    /// type-tagged encoder.
    Null,
    /// Type-tagged SQL `NULL`. Sent on the wire as a `NULL` of the
    /// matching Postgres type, so positional binds into non-INT8
    /// columns work without a SQL cast. On decode, all NULLs land
    /// in [`Self::Null`] regardless of how they were bound.
    TypedNull(PgType),
    /// Postgres `BOOL`.
    Bool(bool),
    /// Any signed Postgres integer (`INT2` / `INT4` / `INT8`) widened
    /// to `i64`. On bind, sent as `INT8`.
    I64(i64),
    /// Postgres `FLOAT4` / `FLOAT8`, widened to `f64` on decode. On
    /// bind, sent as `FLOAT8`.
    F64(f64),
    /// Postgres `TEXT` / `VARCHAR` / `CHAR` / `BPCHAR` / `NAME`.
    String(String),
    /// Postgres `BYTEA`.
    Bytes(Vec<u8>),
    /// Postgres `UUID`. Available when the `uuid` feature is on.
    #[cfg(feature = "uuid")]
    Uuid(uuid::Uuid),
    /// Postgres `JSON` or `JSONB`. Available when the `json` feature
    /// is on. Both Postgres types decode to the same `serde_json::Value`;
    /// on bind, the value is sent as `JSONB`.
    #[cfg(feature = "json")]
    Json(serde_json::Value),
    /// Postgres `NUMERIC`. Available when the `numeric` feature is
    /// on. `rust_decimal::Decimal` carries up to 28-29 significant
    /// digits; values past that range round-trip lossily.
    #[cfg(feature = "numeric")]
    Numeric(rust_decimal::Decimal),
    /// Postgres `TIMESTAMP` (without time zone). Available when the
    /// `time` feature is on.
    #[cfg(feature = "time")]
    Timestamp(time::PrimitiveDateTime),
    /// Postgres `TIMESTAMPTZ` (with time zone). Available when the
    /// `time` feature is on.
    #[cfg(feature = "time")]
    TimestampTz(time::OffsetDateTime),
    /// Postgres `DATE`. Available when the `time` feature is on.
    #[cfg(feature = "time")]
    Date(time::Date),
}

impl PgValue {
    /// `true` iff the value is `Null` or `TypedNull(_)`. Both are
    /// SQL NULL on the wire; the difference is only the encoder
    /// hint at bind time.
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null | Self::TypedNull(_))
    }

    /// Build a typed NULL for `pg_type`. Same as
    /// `PgValue::TypedNull(pg_type)`, kept for builder-style use.
    pub fn typed_null(pg_type: PgType) -> Self {
        Self::TypedNull(pg_type)
    }

    /// Convenience: typed NULL for `BOOL`.
    pub fn null_bool() -> Self {
        Self::TypedNull(PgType::Bool)
    }

    /// Convenience: typed NULL for `INT8`.
    pub fn null_i64() -> Self {
        Self::TypedNull(PgType::I64)
    }

    /// Convenience: typed NULL for `FLOAT8`.
    pub fn null_f64() -> Self {
        Self::TypedNull(PgType::F64)
    }

    /// Convenience: typed NULL for `TEXT` (also matches
    /// `VARCHAR` / `BPCHAR` / `CITEXT`).
    pub fn null_text() -> Self {
        Self::TypedNull(PgType::Text)
    }

    /// Convenience: typed NULL for `BYTEA`.
    pub fn null_bytes() -> Self {
        Self::TypedNull(PgType::Bytes)
    }

    /// Convenience: typed NULL for `UUID`.
    #[cfg(feature = "uuid")]
    pub fn null_uuid() -> Self {
        Self::TypedNull(PgType::Uuid)
    }

    /// Convenience: typed NULL for `JSONB` (also accepted by
    /// `JSON` columns).
    #[cfg(feature = "json")]
    pub fn null_json() -> Self {
        Self::TypedNull(PgType::Json)
    }

    /// Convenience: typed NULL for `NUMERIC`.
    #[cfg(feature = "numeric")]
    pub fn null_numeric() -> Self {
        Self::TypedNull(PgType::Numeric)
    }

    /// Convenience: typed NULL for `TIMESTAMP`.
    #[cfg(feature = "time")]
    pub fn null_timestamp() -> Self {
        Self::TypedNull(PgType::Timestamp)
    }

    /// Convenience: typed NULL for `TIMESTAMPTZ`.
    #[cfg(feature = "time")]
    pub fn null_timestamptz() -> Self {
        Self::TypedNull(PgType::TimestampTz)
    }

    /// Convenience: typed NULL for `DATE`.
    #[cfg(feature = "time")]
    pub fn null_date() -> Self {
        Self::TypedNull(PgType::Date)
    }

    /// Returns the inner bool if this is a `Bool`. None otherwise.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Returns the inner `i64` if this is an `I64`. None otherwise.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::I64(i) => Some(*i),
            _ => None,
        }
    }

    /// Returns the inner `f64` if this is an `F64`. None otherwise.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::F64(f) => Some(*f),
            _ => None,
        }
    }

    /// Returns the inner string slice if this is a `String`. None
    /// otherwise.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Returns the inner bytes if this is a `Bytes`. None otherwise.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes(b) => Some(b.as_slice()),
            _ => None,
        }
    }

    /// Returns the inner [`uuid::Uuid`] if this is `Uuid`. None
    /// otherwise.
    #[cfg(feature = "uuid")]
    pub fn as_uuid(&self) -> Option<uuid::Uuid> {
        match self {
            Self::Uuid(u) => Some(*u),
            _ => None,
        }
    }

    /// Borrow the inner [`serde_json::Value`] if this is `Json`. None
    /// otherwise.
    #[cfg(feature = "json")]
    pub fn as_json(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Json(v) => Some(v),
            _ => None,
        }
    }

    /// Returns the inner [`rust_decimal::Decimal`] if this is
    /// `Numeric`. None otherwise.
    #[cfg(feature = "numeric")]
    pub fn as_numeric(&self) -> Option<rust_decimal::Decimal> {
        match self {
            Self::Numeric(d) => Some(*d),
            _ => None,
        }
    }

    /// Returns the inner [`time::PrimitiveDateTime`] if this is
    /// `Timestamp`. None otherwise.
    #[cfg(feature = "time")]
    pub fn as_timestamp(&self) -> Option<time::PrimitiveDateTime> {
        match self {
            Self::Timestamp(t) => Some(*t),
            _ => None,
        }
    }

    /// Returns the inner [`time::OffsetDateTime`] if this is
    /// `TimestampTz`. None otherwise.
    #[cfg(feature = "time")]
    pub fn as_timestamptz(&self) -> Option<time::OffsetDateTime> {
        match self {
            Self::TimestampTz(t) => Some(*t),
            _ => None,
        }
    }

    /// Returns the inner [`time::Date`] if this is `Date`. None
    /// otherwise.
    #[cfg(feature = "time")]
    pub fn as_date(&self) -> Option<time::Date> {
        match self {
            Self::Date(d) => Some(*d),
            _ => None,
        }
    }

    /// Consumes the value, returning the owned `String` if this is
    /// `String`. None otherwise. Avoids the clone in [`Self::as_str`].
    pub fn into_string(self) -> Option<String> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    /// Consumes the value, returning the owned `Vec<u8>` if this is
    /// `Bytes`. None otherwise. Avoids the clone in [`Self::as_bytes`].
    pub fn into_bytes(self) -> Option<Vec<u8>> {
        match self {
            Self::Bytes(b) => Some(b),
            _ => None,
        }
    }
}

impl From<bool> for PgValue {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

impl From<i64> for PgValue {
    fn from(v: i64) -> Self {
        Self::I64(v)
    }
}

impl From<i32> for PgValue {
    fn from(v: i32) -> Self {
        Self::I64(i64::from(v))
    }
}

impl From<i16> for PgValue {
    fn from(v: i16) -> Self {
        Self::I64(i64::from(v))
    }
}

impl From<i8> for PgValue {
    fn from(v: i8) -> Self {
        Self::I64(i64::from(v))
    }
}

impl From<u32> for PgValue {
    fn from(v: u32) -> Self {
        Self::I64(i64::from(v))
    }
}

impl From<u16> for PgValue {
    fn from(v: u16) -> Self {
        Self::I64(i64::from(v))
    }
}

impl From<u8> for PgValue {
    fn from(v: u8) -> Self {
        Self::I64(i64::from(v))
    }
}

/// Postgres `INT8` is signed 64-bit; `u64` values past `i64::MAX` would
/// silently change. Expose this as `TryFrom` so the failure is visible
/// at the call site.
impl TryFrom<u64> for PgValue {
    type Error = U64TooLarge;

    fn try_from(v: u64) -> Result<Self, Self::Error> {
        if v <= i64::MAX as u64 {
            Ok(Self::I64(v as i64))
        } else {
            Err(U64TooLarge(v))
        }
    }
}

/// `u64` exceeded `i64::MAX`; cannot be stored as a Postgres `INT8`
/// without loss. Use `Bytes` if you need the full range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct U64TooLarge(pub u64);

impl std::fmt::Display for U64TooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "u64 value {} exceeds i64::MAX", self.0)
    }
}

impl std::error::Error for U64TooLarge {}

impl From<f64> for PgValue {
    fn from(v: f64) -> Self {
        Self::F64(v)
    }
}

impl From<f32> for PgValue {
    fn from(v: f32) -> Self {
        Self::F64(f64::from(v))
    }
}

impl From<String> for PgValue {
    fn from(v: String) -> Self {
        Self::String(v)
    }
}

impl From<&str> for PgValue {
    fn from(v: &str) -> Self {
        Self::String(v.to_string())
    }
}

impl From<Vec<u8>> for PgValue {
    fn from(v: Vec<u8>) -> Self {
        Self::Bytes(v)
    }
}

impl From<&[u8]> for PgValue {
    fn from(v: &[u8]) -> Self {
        Self::Bytes(v.to_vec())
    }
}

impl<const N: usize> From<&[u8; N]> for PgValue {
    fn from(v: &[u8; N]) -> Self {
        Self::Bytes(v.to_vec())
    }
}

#[cfg(feature = "uuid")]
impl From<uuid::Uuid> for PgValue {
    fn from(v: uuid::Uuid) -> Self {
        Self::Uuid(v)
    }
}

#[cfg(feature = "json")]
impl From<serde_json::Value> for PgValue {
    fn from(v: serde_json::Value) -> Self {
        Self::Json(v)
    }
}

#[cfg(feature = "numeric")]
impl From<rust_decimal::Decimal> for PgValue {
    fn from(v: rust_decimal::Decimal) -> Self {
        Self::Numeric(v)
    }
}

#[cfg(feature = "time")]
impl From<time::PrimitiveDateTime> for PgValue {
    fn from(v: time::PrimitiveDateTime) -> Self {
        Self::Timestamp(v)
    }
}

#[cfg(feature = "time")]
impl From<time::OffsetDateTime> for PgValue {
    fn from(v: time::OffsetDateTime) -> Self {
        Self::TimestampTz(v)
    }
}

#[cfg(feature = "time")]
impl From<time::Date> for PgValue {
    fn from(v: time::Date) -> Self {
        Self::Date(v)
    }
}

impl<T> From<Option<T>> for PgValue
where
    T: Into<PgValue>,
{
    fn from(v: Option<T>) -> Self {
        match v {
            Some(t) => t.into(),
            None => PgValue::Null,
        }
    }
}

impl From<PgType> for PgValue {
    /// Construct a typed NULL: `PgType::Uuid.into()` returns
    /// `PgValue::TypedNull(PgType::Uuid)`.
    fn from(pg_type: PgType) -> Self {
        Self::TypedNull(pg_type)
    }
}

/// One bridged Postgres operation.
///
/// Multi-statement SQL is not validated here; SQLx prepares one
/// statement per query and rejects multi-statement bodies with
/// [`PgError::Sqlx`]. Use one statement per request.
#[derive(Debug, Clone)]
pub enum PgRequest {
    /// Run one row-less statement. Reply carries `rows_affected`.
    Execute {
        /// SQL text.
        sql: String,
        /// Positional parameters bound in order.
        params: Vec<PgValue>,
    },
    /// Run one query expected to return zero or one row.
    ///
    /// Past one row, the worker surfaces [`PgError::TooManyRows`]
    /// rather than silently picking one. Use SQL `LIMIT 1` if your
    /// query may return more rows than you care about.
    FetchOne {
        /// SQL text.
        sql: String,
        /// Positional parameters bound in order.
        params: Vec<PgValue>,
    },
    /// Run one query and stream rows up to a cap.
    ///
    /// The bridge pulls at most `min(max_rows, config.max_response_rows) + 1`
    /// rows from SQLx. Past the effective cap, it stops pulling and
    /// returns [`PgResponse::Rows`] with `truncated = true` — never
    /// growing the bridge's resident set past the cap.
    FetchMany {
        /// SQL text.
        sql: String,
        /// Positional parameters bound in order.
        params: Vec<PgValue>,
        /// Caller's row cap. Bridge applies the lesser of this and
        /// [`PgConfig::max_response_rows`].
        max_rows: usize,
    },
    /// Run an atomic script of [`PgStep`]s inside one transaction.
    ///
    /// Bridge takes one connection from the pool, opens a SQLx
    /// `Transaction`, runs each step in order, and either commits
    /// (all steps succeeded) or rolls back (first failing step). The
    /// reply is a single [`PgResponse::Transaction`] carrying the
    /// per-step outcomes plus commit/rollback truth. No nested
    /// transactions; nesting is rejected at admission with
    /// [`PgError::InvalidRequest`]. Empty `steps` is also rejected.
    ///
    /// Counts as one slot against `max_in_flight`, holds one
    /// connection for the script's wall-clock duration. The bridge's
    /// per-attempt deadline applies to the script as a whole.
    Transaction {
        /// Ordered list of statements. Must be non-empty.
        steps: Vec<PgStep>,
    },
}

/// One statement inside a [`PgRequest::Transaction`] script. The
/// shapes mirror [`PgRequest`] minus `Transaction` — nested
/// transactions are rejected at admission.
#[derive(Debug, Clone)]
pub enum PgStep {
    /// Run one row-less statement.
    Execute {
        /// SQL text.
        sql: String,
        /// Positional parameters bound in order.
        params: Vec<PgValue>,
    },
    /// Run one query expected to return zero or one row.
    FetchOne {
        /// SQL text.
        sql: String,
        /// Positional parameters bound in order.
        params: Vec<PgValue>,
    },
    /// Run one query and stream rows up to a cap.
    FetchMany {
        /// SQL text.
        sql: String,
        /// Positional parameters bound in order.
        params: Vec<PgValue>,
        /// Caller's row cap; clamped by `PgConfig::max_response_rows`.
        max_rows: usize,
    },
}

impl PgStep {
    /// `Execute` step with no parameters.
    pub fn execute(sql: impl Into<String>) -> Self {
        Self::Execute {
            sql: sql.into(),
            params: Vec::new(),
        }
    }

    /// `FetchOne` step with no parameters.
    pub fn fetch_one(sql: impl Into<String>) -> Self {
        Self::FetchOne {
            sql: sql.into(),
            params: Vec::new(),
        }
    }

    /// `FetchMany` step with no parameters and a per-step row cap.
    pub fn fetch_many(sql: impl Into<String>, max_rows: usize) -> Self {
        Self::FetchMany {
            sql: sql.into(),
            params: Vec::new(),
            max_rows,
        }
    }

    /// Append one parameter. Chainable.
    pub fn param(mut self, value: impl Into<PgValue>) -> Self {
        match &mut self {
            Self::Execute { params, .. }
            | Self::FetchOne { params, .. }
            | Self::FetchMany { params, .. } => {
                params.push(value.into());
            }
        }
        self
    }

    /// Replace the parameter list.
    pub fn params(mut self, values: Vec<PgValue>) -> Self {
        match &mut self {
            Self::Execute { params, .. }
            | Self::FetchOne { params, .. }
            | Self::FetchMany { params, .. } => {
                *params = values;
            }
        }
        self
    }

    /// Borrow the SQL text. Used by validation.
    pub(crate) fn sql(&self) -> &str {
        match self {
            Self::Execute { sql, .. }
            | Self::FetchOne { sql, .. }
            | Self::FetchMany { sql, .. } => sql,
        }
    }

    /// Borrow the parameter slice. Used by validation.
    pub(crate) fn params_slice(&self) -> &[PgValue] {
        match self {
            Self::Execute { params, .. }
            | Self::FetchOne { params, .. }
            | Self::FetchMany { params, .. } => params,
        }
    }
}

impl PgRequest {
    /// `Execute` request with no parameters.
    pub fn execute(sql: impl Into<String>) -> Self {
        Self::Execute {
            sql: sql.into(),
            params: Vec::new(),
        }
    }

    /// `FetchOne` request with no parameters.
    pub fn fetch_one(sql: impl Into<String>) -> Self {
        Self::FetchOne {
            sql: sql.into(),
            params: Vec::new(),
        }
    }

    /// `FetchMany` request with no parameters and a per-call row cap.
    ///
    /// Effective cap is the lesser of `max_rows` and
    /// [`PgConfig::max_response_rows`].
    pub fn fetch_many(sql: impl Into<String>, max_rows: usize) -> Self {
        Self::FetchMany {
            sql: sql.into(),
            params: Vec::new(),
            max_rows,
        }
    }

    /// Append one parameter. Chainable.
    ///
    /// ```
    /// use tina_sqlx_bridge::{PgRequest, PgValue};
    /// let req = PgRequest::execute("INSERT INTO t (k, v) VALUES ($1, $2)")
    ///     .param(1)
    ///     .param("hello");
    /// match req {
    ///     PgRequest::Execute { params, .. } => {
    ///         assert_eq!(params.len(), 2);
    ///         assert_eq!(params[0], PgValue::I64(1));
    ///     }
    ///     _ => unreachable!(),
    /// }
    /// ```
    pub fn param(mut self, value: impl Into<PgValue>) -> Self {
        match &mut self {
            Self::Execute { params, .. }
            | Self::FetchOne { params, .. }
            | Self::FetchMany { params, .. } => {
                params.push(value.into());
            }
            // Transaction has no top-level params; build steps with
            // `PgStep::param` and pass them via `.steps(...)`.
            Self::Transaction { .. } => {}
        }
        self
    }

    /// **Replace** the parameter list. Anything previously added by
    /// [`Self::param`] is discarded. No-op on `Transaction` — build
    /// steps and pass them via [`Self::steps`] instead.
    pub fn params(mut self, values: Vec<PgValue>) -> Self {
        match &mut self {
            Self::Execute { params, .. }
            | Self::FetchOne { params, .. }
            | Self::FetchMany { params, .. } => {
                *params = values;
            }
            Self::Transaction { .. } => {}
        }
        self
    }

    /// Build a transaction request from an ordered list of steps.
    pub fn transaction(steps: Vec<PgStep>) -> Self {
        Self::Transaction { steps }
    }

    /// Replace the steps of a `Transaction` request. No-op on
    /// non-Transaction variants.
    pub fn steps(mut self, new_steps: Vec<PgStep>) -> Self {
        if let Self::Transaction { steps } = &mut self {
            *steps = new_steps;
        }
        self
    }
}

/// One terminal Postgres reply.
#[derive(Debug, Clone, PartialEq)]
pub enum PgResponse {
    /// `Execute` outcome.
    Executed {
        /// Rows affected, as reported by Postgres.
        rows_affected: u64,
    },
    /// `FetchOne` outcome with exactly one row.
    Row(PgRow),
    /// `FetchOne` matched zero rows.
    NoRows,
    /// `FetchMany` outcome. Rows are buffered up to the effective
    /// cap; `truncated = true` means there were more rows on the
    /// wire that the bridge stopped pulling.
    Rows {
        /// Buffered rows, in result order.
        rows: Vec<PgRow>,
        /// `true` if Postgres had more rows to send and the bridge
        /// hit its cap. The buffered rows are still valid.
        truncated: bool,
    },
    /// `Transaction` outcome. Either every step committed or the
    /// first failing step rolled the whole script back.
    Transaction(PgTransactionOutcome),
}

/// Result of a transactional script.
#[derive(Debug, Clone, PartialEq)]
pub enum PgTransactionOutcome {
    /// All steps succeeded; the bridge ran `COMMIT`.
    Committed {
        /// Per-step outcomes in input order.
        steps: Vec<PgStepOk>,
    },
    /// At least one step failed; the bridge ran `ROLLBACK`. Records
    /// the steps that completed before the failure plus the failing
    /// step's index and error.
    RolledBack {
        /// Per-step outcomes for steps before the failure.
        completed: Vec<PgStepOk>,
        /// Index into the original `steps` of the failing step.
        failed_at: usize,
        /// Error returned by the failing step.
        error: PgError,
    },
    /// Every step succeeded, then `COMMIT` returned an error. From
    /// the caller's view the transaction outcome is ambiguous: the
    /// listed steps definitely ran before the commit attempt, but the
    /// bridge cannot honestly claim committed or rolled back.
    CommitAmbiguous {
        /// Per-step outcomes in input order before the commit attempt.
        completed: Vec<PgStepOk>,
        /// Error returned by `COMMIT`.
        error: PgError,
    },
}

/// Successful outcome of a single [`PgStep`].
#[derive(Debug, Clone, PartialEq)]
pub enum PgStepOk {
    /// `Execute` succeeded with this many rows affected.
    Executed {
        /// Rows affected, as reported by Postgres.
        rows_affected: u64,
    },
    /// `FetchOne` returned exactly one row.
    Row(PgRow),
    /// `FetchOne` returned zero rows.
    NoRows,
    /// `FetchMany` returned a buffered set, possibly truncated.
    Rows {
        /// Buffered rows, in result order.
        rows: Vec<PgRow>,
        /// Set when the bridge hit its row cap.
        truncated: bool,
    },
}

/// One Postgres result row plus its column names.
#[derive(Debug, Clone, PartialEq)]
pub struct PgRow {
    /// Column names in result order.
    pub columns: Vec<String>,
    /// Cells in column order.
    pub cells: Vec<PgValue>,
}

impl PgRow {
    /// Number of columns.
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// `true` iff no columns.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Borrow cell `idx`. None if out of range.
    pub fn col(&self, idx: usize) -> Option<&PgValue> {
        self.cells.get(idx)
    }

    /// Borrow the cell whose column has this name. None if no such
    /// column. Linear scan; fine for small result rows.
    pub fn by_name(&self, name: &str) -> Option<&PgValue> {
        self.columns
            .iter()
            .position(|c| c == name)
            .and_then(|i| self.cells.get(i))
    }

    /// Read column `idx` as `i64`. None if missing column, NULL, or
    /// not an integer cell.
    pub fn get_i64(&self, idx: usize) -> Option<i64> {
        self.col(idx).and_then(PgValue::as_i64)
    }

    /// Read column `idx` as a string slice. None if missing column,
    /// NULL, or not a text cell.
    pub fn get_text(&self, idx: usize) -> Option<&str> {
        self.col(idx).and_then(PgValue::as_str)
    }

    /// Read column `idx` as `bool`. None if missing column, NULL, or
    /// not a bool cell.
    pub fn get_bool(&self, idx: usize) -> Option<bool> {
        self.col(idx).and_then(PgValue::as_bool)
    }

    /// Read column `idx` as `f64`. None if missing column, NULL, or
    /// not a float cell.
    pub fn get_f64(&self, idx: usize) -> Option<f64> {
        self.col(idx).and_then(PgValue::as_f64)
    }

    /// Read column `idx` as bytes. None if missing column, NULL, or
    /// not a bytea cell.
    pub fn get_bytes(&self, idx: usize) -> Option<&[u8]> {
        self.col(idx).and_then(PgValue::as_bytes)
    }

    /// Read column `idx` as `uuid::Uuid`. None if missing column,
    /// NULL, or not a uuid cell.
    #[cfg(feature = "uuid")]
    pub fn get_uuid(&self, idx: usize) -> Option<uuid::Uuid> {
        self.col(idx).and_then(PgValue::as_uuid)
    }

    /// Borrow column `idx` as `serde_json::Value`. None if missing
    /// column, NULL, or not a json/jsonb cell.
    #[cfg(feature = "json")]
    pub fn get_json(&self, idx: usize) -> Option<&serde_json::Value> {
        self.col(idx).and_then(PgValue::as_json)
    }

    /// Read column `idx` as `rust_decimal::Decimal`. None if missing
    /// column, NULL, or not a numeric cell.
    #[cfg(feature = "numeric")]
    pub fn get_numeric(&self, idx: usize) -> Option<rust_decimal::Decimal> {
        self.col(idx).and_then(PgValue::as_numeric)
    }

    /// Read column `idx` as `time::PrimitiveDateTime`. None if
    /// missing column, NULL, or not a timestamp cell.
    #[cfg(feature = "time")]
    pub fn get_timestamp(&self, idx: usize) -> Option<time::PrimitiveDateTime> {
        self.col(idx).and_then(PgValue::as_timestamp)
    }

    /// Read column `idx` as `time::OffsetDateTime`. None if missing
    /// column, NULL, or not a timestamptz cell.
    #[cfg(feature = "time")]
    pub fn get_timestamptz(&self, idx: usize) -> Option<time::OffsetDateTime> {
        self.col(idx).and_then(PgValue::as_timestamptz)
    }

    /// Read column `idx` as `time::Date`. None if missing column,
    /// NULL, or not a date cell.
    #[cfg(feature = "time")]
    pub fn get_date(&self, idx: usize) -> Option<time::Date> {
        self.col(idx).and_then(PgValue::as_date)
    }
}

/// Worker-side outcome of one bridged Postgres call.
///
/// Layered with the runtime's `CallOutcome`: ingress / IsolateCall
/// failures stay on `CallOutcome`, worker errors come here. The bridge
/// never collapses the two layers silently — see crate docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PgError {
    /// Bridge `max_in_flight` cap rejected admission. The SQLx pool
    /// was never asked. Distinct from [`Self::PoolAcquireTimeout`].
    Full,
    /// Bridge has been closed and rejects new work. The SQLx pool is
    /// not necessarily closed.
    Closed,
    /// Bridge per-attempt deadline elapsed. The spawned SQLx future
    /// is detached (the bridge drops its result receiver) and runs
    /// to natural completion; the eventual outcome is tallied and
    /// `late_results` increments. The bridge does **not** issue a
    /// Postgres `CancelRequest`, so the query keeps running on the
    /// server and the connection stays held until the future
    /// returns. Distinct from `CallOutcome::Timeout`, which means
    /// the *caller's* IsolateCall deadline elapsed.
    Timeout,
    /// SQLx pool could not acquire a connection within its own
    /// `acquire_timeout`. Tina admission was fine; the bottleneck was
    /// the SQLx pool. Distinct from [`Self::Full`].
    PoolAcquireTimeout,
    /// SQLx pool has been closed. Either the bridge was constructed
    /// with a pool that was later closed, or the bridge dropped its
    /// owned pool.
    PoolClosed,
    /// Rejected before the spawned task ran (empty SQL, too many
    /// params).
    InvalidRequest(String),
    /// `FetchOne` matched more than one row. The bridge does not pick
    /// one; it surfaces this so the caller can add `LIMIT 1` or use a
    /// future `FetchMany`.
    TooManyRows,
    /// Row decoding failed. The bridge supports a narrow set of
    /// Postgres column types in first form; everything else lands here
    /// with the type name.
    Decode(String),
    /// Catch-all SQLx / Postgres error with the underlying message
    /// preserved.
    Sqlx(String),
    /// Bridge invariant failed (e.g. spawned task ended without a
    /// result). Never used to hide a SQLx error.
    Internal(String),
}

impl std::fmt::Display for PgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => f.write_str("pg worker: bounded ingress full"),
            Self::Closed => f.write_str("pg worker: closed"),
            Self::Timeout => f.write_str("pg worker: per-attempt timeout"),
            Self::PoolAcquireTimeout => f.write_str("pg worker: SQLx pool acquire timeout"),
            Self::PoolClosed => f.write_str("pg worker: SQLx pool closed"),
            Self::InvalidRequest(msg) => write!(f, "pg worker: invalid request: {msg}"),
            Self::TooManyRows => f.write_str("pg worker: FetchOne matched more than one row"),
            Self::Decode(msg) => write!(f, "pg worker: decode error: {msg}"),
            Self::Sqlx(msg) => write!(f, "pg worker: sqlx error: {msg}"),
            Self::Internal(msg) => write!(f, "pg worker: internal: {msg}"),
        }
    }
}

impl std::error::Error for PgError {}

/// Reasons a [`PgConfig`] cannot produce a working worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PgConfigError {
    /// `mailbox_capacity == 0`.
    ZeroMailboxCapacity,
    /// `mailbox_capacity` over the hard cap.
    MailboxCapacityTooLarge {
        /// Requested capacity.
        requested: usize,
        /// Hard cap.
        cap: usize,
    },
    /// `max_in_flight == 0`.
    ZeroMaxInFlight,
    /// `default_timeout == 0`.
    ZeroDefaultTimeout,
    /// `poll_interval == 0`. The wakeup loop would never yield.
    ZeroPollInterval,
    /// `max_request_params == 0`.
    ZeroMaxRequestParams,
    /// `max_response_rows == 0`. Use a request `max_rows` of `0` for
    /// "no rows expected"; the config ceiling must be `>= 1`.
    ZeroMaxResponseRows,
    /// `cancel.pool_size == 0`. The cancel sidecar needs at least one
    /// connection to fire `pg_cancel_backend`.
    ZeroCancelPoolSize,
    /// `cancel.acquire_timeout == 0`. SQLx would reject every cancel
    /// acquire immediately.
    ZeroCancelAcquireTimeout,
    /// SQLx pool config requested zero connections. SQLx requires at
    /// least one. Distinct from a "no public cap" error: pool size is
    /// caller-owned.
    ZeroPoolMaxConnections,
    /// SQLx pool acquire timeout is `Duration::ZERO`. SQLx would
    /// reject every acquire immediately, so the bridge would always
    /// return [`PgError::PoolAcquireTimeout`].
    ZeroPoolAcquireTimeout,
}

impl std::fmt::Display for PgConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroMailboxCapacity => f.write_str("mailbox_capacity must be >= 1"),
            Self::MailboxCapacityTooLarge { requested, cap } => {
                write!(f, "mailbox_capacity {requested} exceeds cap {cap}")
            }
            Self::ZeroMaxInFlight => f.write_str("max_in_flight must be >= 1"),
            Self::ZeroDefaultTimeout => f.write_str("default_timeout must be > 0"),
            Self::ZeroPollInterval => f.write_str("poll_interval must be > 0"),
            Self::ZeroMaxRequestParams => f.write_str("max_request_params must be >= 1"),
            Self::ZeroMaxResponseRows => f.write_str("max_response_rows must be >= 1"),
            Self::ZeroCancelPoolSize => f.write_str("cancel.pool_size must be >= 1"),
            Self::ZeroCancelAcquireTimeout => f.write_str("cancel.acquire_timeout must be > 0"),
            Self::ZeroPoolMaxConnections => f.write_str("pool max_connections must be >= 1"),
            Self::ZeroPoolAcquireTimeout => f.write_str("pool acquire_timeout must be > 0"),
        }
    }
}

impl std::error::Error for PgConfigError {}

/// Sidecar cancel-pool settings.
///
/// Compatibility-only. The old sidecar path fired `pg_cancel_backend(pid)` by
/// captured backend PID; that can race connection reuse unless the exact
/// connection is quarantined. The bridge still accepts and validates this shape
/// so existing config files parse, but it does not build a sidecar or send DB
/// cancels.
#[derive(Debug, Clone)]
pub struct PgCancelConfig {
    /// Sidecar pool size. Tiny by default — cancel calls are short
    /// and rare.
    pub pool_size: u32,
    /// SQLx `acquire_timeout` for the sidecar pool. The bridge does
    /// not wait on the cancel's own deadline directly; this just
    /// bounds how long the sidecar will hold a fire-and-forget cancel
    /// task.
    pub acquire_timeout: Duration,
}

impl PgCancelConfig {
    /// Cancel sidecar with conservative defaults (`pool_size = 1`,
    /// `acquire_timeout = 2s`).
    pub fn new() -> Self {
        Self {
            pool_size: 1,
            acquire_timeout: Duration::from_secs(2),
        }
    }

    /// Sidecar pool size.
    pub fn with_pool_size(mut self, n: u32) -> Self {
        self.pool_size = n;
        self
    }

    /// Sidecar acquire timeout.
    pub fn with_acquire_timeout(mut self, t: Duration) -> Self {
        self.acquire_timeout = t;
        self
    }
}

impl Default for PgCancelConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// SQLx pool settings the bridge will apply when it builds its own
/// pool. Ignored when the bridge is given a caller-supplied
/// `sqlx::PgPool`.
#[derive(Debug, Clone)]
pub struct PgPoolConfig {
    /// Postgres connection URL (`postgres://user:pass@host/db`).
    pub url: String,
    /// SQLx `max_connections`. Maps directly to
    /// `sqlx::pool::PoolOptions::max_connections`.
    pub max_connections: u32,
    /// SQLx `acquire_timeout`. How long the pool waits for an
    /// available connection before returning
    /// [`PgError::PoolAcquireTimeout`].
    pub acquire_timeout: Duration,
}

impl PgPoolConfig {
    /// Pool with conservative defaults (`max_connections = 4`,
    /// `acquire_timeout = 5s`).
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            max_connections: 4,
            acquire_timeout: Duration::from_secs(5),
        }
    }

    /// Sets `max_connections`.
    pub fn with_max_connections(mut self, n: u32) -> Self {
        self.max_connections = n;
        self
    }

    /// Sets `acquire_timeout`.
    pub fn with_acquire_timeout(mut self, t: Duration) -> Self {
        self.acquire_timeout = t;
        self
    }
}

/// Bridge worker configuration.
///
/// Every knob is named. No silent clamps; out-of-range values reject
/// through [`PgConfig::validate`].
///
/// ## Tina caps vs SQLx pool caps
///
/// - `mailbox_capacity`, `max_in_flight`, `default_timeout`,
///   `poll_interval`, `max_request_params` belong to Tina. They cap
///   how much pressure reaches the SQLx pool in the first place.
/// - `pool` (when present) belongs to SQLx. Its `max_connections` and
///   `acquire_timeout` say what happens once Tina has admitted a
///   request.
///
/// Both matter and both are visible. A `Full` from Tina is not a
/// `PoolAcquireTimeout` from SQLx, even though both mean "no work
/// happened."
#[derive(Debug, Clone)]
pub struct PgConfig {
    /// SQLx pool settings. `None` means the worker must be installed
    /// with a caller-supplied `sqlx::PgPool` via
    /// [`crate::PgWorker::install_with_pool`]; passing it to the
    /// config-built [`crate::PgWorker::install`] path returns
    /// [`crate::InstallError::MissingPoolConfig`].
    pub pool: Option<PgPoolConfig>,
    /// Worker mailbox capacity (Tina ingress). Past this, callers see
    /// `CallError::TargetFull` at the runtime layer. Hard-capped at
    /// `2^20`.
    pub mailbox_capacity: usize,
    /// Max simultaneously-in-flight bridge operations. Past this, the
    /// worker replies [`PgError::Full`] before SQLx ever sees the
    /// request.
    pub max_in_flight: usize,
    /// Per-attempt deadline at the bridge layer. Past this, the
    /// bridge surfaces [`PgError::Timeout`] to the caller but keeps
    /// the external SQLx slot leased until the spawned task reaches
    /// terminal; late completion counts in `late_results`.
    pub default_timeout: Duration,
    /// Poll interval for the worker-result wakeup loop. Smaller =
    /// lower latency, more trace chatter.
    pub poll_interval: Duration,
    /// Max positional params per request. Over the cap rejects with
    /// [`PgError::InvalidRequest`] before the spawned task runs.
    pub max_request_params: usize,
    /// Hard ceiling on rows the bridge will buffer for a single
    /// [`PgRequest::FetchMany`]. The request's own `max_rows` is
    /// further capped to this value, so a caller cannot ask for
    /// `usize::MAX` and force unbounded buffering.
    pub max_response_rows: usize,
    /// Compatibility field for the disabled DB-side cancel sidecar. The old
    /// implementation could race PID reuse and cancel a later query. Default
    /// `None`; `Some` validates but is not honored until a quarantine-based
    /// cancel path exists.
    ///
    /// **Only honored on [`crate::PgWorker::install`].** The
    /// supplied-pool path ([`crate::PgWorker::install_with_pool`])
    /// silently ignores this field — the bridge cannot construct a
    /// sidecar pool without an owned URL, and the caller already
    /// owns connection lifetimes.
    ///
    /// `db_cancels_sent` remains zero. Tina-side timeout means "stop
    /// waiting"; Postgres may keep running.
    pub cancel: Option<PgCancelConfig>,
}

impl PgConfig {
    /// Conservative defaults targeting "small, visible, bounded."
    /// `pool` is `None`; either set it via [`Self::with_pool`] or use
    /// [`crate::PgWorker::install_with_pool`].
    pub fn new() -> Self {
        Self {
            pool: None,
            mailbox_capacity: 64,
            max_in_flight: 8,
            default_timeout: Duration::from_secs(5),
            poll_interval: Duration::from_millis(2),
            max_request_params: 64,
            max_response_rows: 4096,
            cancel: None,
        }
    }

    /// Conservative defaults around an SQLx pool config built from
    /// the supplied URL.
    pub fn from_url(url: impl Into<String>) -> Self {
        Self::new().with_pool(PgPoolConfig::new(url))
    }

    /// Conservative defaults intended for the supplied-pool install
    /// path. `pool` stays `None`.
    pub fn bridge_only() -> Self {
        Self::new()
    }

    /// Sets `pool` so [`crate::PgWorker::install`] can build a pool.
    pub fn with_pool(mut self, pool: PgPoolConfig) -> Self {
        self.pool = Some(pool);
        self
    }

    /// Sets `mailbox_capacity`.
    pub fn with_mailbox_capacity(mut self, capacity: usize) -> Self {
        self.mailbox_capacity = capacity;
        self
    }

    /// Sets `max_in_flight`.
    pub fn with_max_in_flight(mut self, max: usize) -> Self {
        self.max_in_flight = max;
        self
    }

    /// Sets `default_timeout`.
    pub fn with_default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
        self
    }

    /// Sets `poll_interval`.
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Sets `max_request_params`.
    pub fn with_max_request_params(mut self, max: usize) -> Self {
        self.max_request_params = max;
        self
    }

    /// Sets `max_response_rows`.
    pub fn with_max_response_rows(mut self, max: usize) -> Self {
        self.max_response_rows = max;
        self
    }

    /// Records a future DB-side cancellation preference. This is a
    /// compatibility no-op: the bridge validates the config but does not fire
    /// `pg_cancel_backend(pid)` because the old sidecar path could cancel a
    /// later query on a reused backend PID.
    ///
    /// Tina-side timeout still settles the caller promptly, while
    /// the SQLx slot remains occupied until physical terminal.
    pub fn with_cancel_on_timeout(mut self, pool_size: u32) -> Self {
        self.cancel = Some(PgCancelConfig::new().with_pool_size(pool_size));
        self
    }

    /// Sets the full cancel config explicitly. Use this when you want
    /// to override the sidecar's `acquire_timeout` from
    /// [`PgCancelConfig::default`].
    pub fn with_cancel(mut self, cancel: PgCancelConfig) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Validates the full config — Tina-side fields plus the
    /// embedded `pool` config when it is `Some`. Used by
    /// [`crate::PgWorker::install`] (config-built pool path).
    pub fn validate(&self) -> Result<(), PgConfigError> {
        self.validate_tina()?;
        if let Some(pool) = &self.pool {
            if pool.max_connections == 0 {
                return Err(PgConfigError::ZeroPoolMaxConnections);
            }
            if pool.acquire_timeout.is_zero() {
                return Err(PgConfigError::ZeroPoolAcquireTimeout);
            }
        }
        Ok(())
    }

    /// Validates only the Tina-side fields, ignoring `pool`. Used by
    /// [`crate::PgWorker::install_with_pool`] where the supplied
    /// `sqlx::PgPool` owns its SQLx settings — rejecting on
    /// `PgConfig::pool` here would punish a caller for fields the
    /// bridge promised not to apply.
    pub fn validate_tina(&self) -> Result<(), PgConfigError> {
        if self.mailbox_capacity == 0 {
            return Err(PgConfigError::ZeroMailboxCapacity);
        }
        if self.mailbox_capacity > MAX_MAILBOX_CAPACITY {
            return Err(PgConfigError::MailboxCapacityTooLarge {
                requested: self.mailbox_capacity,
                cap: MAX_MAILBOX_CAPACITY,
            });
        }
        if self.max_in_flight == 0 {
            return Err(PgConfigError::ZeroMaxInFlight);
        }
        if self.default_timeout.is_zero() {
            return Err(PgConfigError::ZeroDefaultTimeout);
        }
        if self.poll_interval.is_zero() {
            return Err(PgConfigError::ZeroPollInterval);
        }
        if self.max_request_params == 0 {
            return Err(PgConfigError::ZeroMaxRequestParams);
        }
        if self.max_response_rows == 0 {
            return Err(PgConfigError::ZeroMaxResponseRows);
        }
        if let Some(cancel) = &self.cancel {
            if cancel.pool_size == 0 {
                return Err(PgConfigError::ZeroCancelPoolSize);
            }
            if cancel.acquire_timeout.is_zero() {
                return Err(PgConfigError::ZeroCancelAcquireTimeout);
            }
        }
        Ok(())
    }
}

impl Default for PgConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Why install failed. Each variant points at one phase of bring-up.
#[derive(Debug)]
pub enum InstallError {
    /// Config rejected by [`PgConfig::validate`].
    Config(PgConfigError),
    /// [`crate::PgWorker::install`] was called without
    /// `config.pool`. Either set the pool config or use
    /// [`crate::PgWorker::install_with_pool`] with a supplied
    /// `sqlx::PgPool`.
    MissingPoolConfig,
    /// Building the SQLx `PgPool` failed (URL parse, TLS setup,
    /// initial connect failure depending on pool options).
    Pool(String),
    /// Building the worker's owned Tokio runtime failed.
    Runtime(String),
    /// Tina runtime registration failed.
    Register(String),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(e) => write!(f, "pg bridge install: {e}"),
            Self::MissingPoolConfig => f.write_str(
                "pg bridge install: PgConfig.pool is None; use install_with_pool or set with_pool",
            ),
            Self::Pool(msg) => write!(f, "pg bridge install: pool: {msg}"),
            Self::Runtime(msg) => write!(f, "pg bridge install: tokio runtime: {msg}"),
            Self::Register(msg) => write!(f, "pg bridge install: register: {msg}"),
        }
    }
}

impl std::error::Error for InstallError {}

impl From<PgConfigError> for InstallError {
    fn from(e: PgConfigError) -> Self {
        Self::Config(e)
    }
}
