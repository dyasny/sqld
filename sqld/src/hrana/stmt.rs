use anyhow::{bail, Result};
use std::collections::HashMap;

use super::conn::ProtocolError;
use super::handshake::Protocol;
use super::proto;
use super::session::ResponseError;
use crate::auth::Authenticated;
use crate::database::{Database, DescribeResponse};
use crate::error::Error as SqldError;
use crate::hrana;
use crate::query::{Params, Query, QueryResponse, Value};
use crate::query_analysis::Statement;

#[derive(thiserror::Error, Debug)]
pub enum StmtError {
    #[error("SQL string could not be parsed: {source}")]
    SqlParse { source: anyhow::Error },
    #[error("SQL string does not contain any statement")]
    SqlNoStmt,
    #[error("SQL string contains more than one statement")]
    SqlManyStmts,
    #[error("Arguments do not match SQL parameters: {source}")]
    ArgsInvalid { source: anyhow::Error },
    #[error("Specifying both positional and named arguments is not supported")]
    ArgsBothPositionalAndNamed,

    #[error("Transaction timed out")]
    TransactionTimeout,
    #[error("Server cannot handle additional transactions")]
    TransactionBusy,
    #[error("SQLite error: {message}")]
    SqliteError {
        source: rusqlite::ffi::Error,
        message: String,
    },
    #[error("SQL input error: {message} (at offset {offset})")]
    SqlInputError {
        source: rusqlite::ffi::Error,
        message: String,
        offset: i32,
    },
}

pub async fn execute_stmt(
    db: &dyn Database,
    auth: Authenticated,
    query: Query,
) -> Result<proto::StmtResult> {
    let (query_result, _) = db.execute_one(query, auth).await?;
    match query_result {
        Ok(query_response) => Ok(proto_stmt_result_from_query_response(query_response)),
        Err(sqld_error) => match stmt_error_from_sqld_error(sqld_error) {
            Ok(stmt_error) => bail!(stmt_error),
            Err(sqld_error) => bail!(sqld_error),
        },
    }
}

pub async fn describe_stmt(
    db: &dyn Database,
    auth: Authenticated,
    sql: String,
) -> Result<proto::DescribeResult> {
    match db.describe(sql, auth).await? {
        Ok(describe_response) => Ok(proto_describe_result_from_describe_response(
            describe_response,
        )),
        Err(sqld_error) => match stmt_error_from_sqld_error(sqld_error) {
            Ok(stmt_error) => bail!(stmt_error),
            Err(sqld_error) => bail!(sqld_error),
        },
    }
}

pub fn proto_stmt_to_query(
    proto_stmt: &proto::Stmt,
    sqls: &HashMap<i32, String>,
    protocol: Protocol,
) -> Result<Query> {
    let sql = proto_sql_to_sql(proto_stmt.sql.as_deref(), proto_stmt.sql_id, sqls, protocol)?;

    let mut stmt_iter = Statement::parse(sql);
    let stmt = match stmt_iter.next() {
        Some(Ok(stmt)) => stmt,
        Some(Err(err)) => bail!(StmtError::SqlParse { source: err }),
        None => bail!(StmtError::SqlNoStmt),
    };

    if stmt_iter.next().is_some() {
        bail!(StmtError::SqlManyStmts)
    }

    let params = if proto_stmt.named_args.is_empty() {
        let values = proto_stmt.args.iter().map(proto_value_to_value).collect();
        Params::Positional(values)
    } else if proto_stmt.args.is_empty() {
        let values = proto_stmt
            .named_args
            .iter()
            .map(|arg| (arg.name.clone(), proto_value_to_value(&arg.value)))
            .collect();
        Params::Named(values)
    } else {
        bail!(StmtError::ArgsBothPositionalAndNamed)
    };

    let want_rows = proto_stmt.want_rows.unwrap_or(true);
    Ok(Query {
        stmt,
        params,
        want_rows,
    })
}

pub fn proto_sql_to_sql<'s>(
    proto_sql: Option<&'s str>,
    proto_sql_id: Option<i32>,
    sqls: &'s HashMap<i32, String>,
    protocol: Protocol,
) -> Result<&'s str> {
    if proto_sql_id.is_some() && protocol < Protocol::Hrana2 {
        bail!(ProtocolError::from_message(
            "`sql_id` can be specified in protocol version 2 and higher"
        ))
    }

    match (proto_sql, proto_sql_id) {
        (Some(sql), None) => Ok(sql),
        (None, Some(sql_id)) => match sqls.get(&sql_id) {
            Some(sql) => Ok(sql),
            None => bail!(ResponseError::SqlNotFound { sql_id }),
        },
        (Some(_), Some(_)) => bail!(ProtocolError::from_message(
            "Either `sql` or `sql_id` are required, but not both"
        )),
        (None, None) => bail!(ProtocolError::from_message(
            "Either `sql` or `sql_id` are required"
        )),
    }
}

pub fn proto_stmt_result_from_query_response(query_response: QueryResponse) -> proto::StmtResult {
    let QueryResponse::ResultSet(result_set) = query_response;
    let proto_cols = result_set
        .columns
        .into_iter()
        .map(|col| proto::Col {
            name: Some(col.name),
            decltype: col.decltype,
        })
        .collect();
    let proto_rows = result_set
        .rows
        .into_iter()
        .map(|row| row.values.into_iter().map(proto::Value::from).collect())
        .collect();
    proto::StmtResult {
        cols: proto_cols,
        rows: proto_rows,
        affected_row_count: result_set.affected_row_count,
        last_insert_rowid: result_set.last_insert_rowid,
    }
}

fn proto_value_to_value(proto_value: &proto::Value) -> Value {
    match proto_value {
        proto::Value::Null => Value::Null,
        proto::Value::Integer { value } => Value::Integer(*value),
        proto::Value::Float { value } => Value::Real(*value),
        proto::Value::Text { value } => Value::Text(value.as_ref().into()),
        proto::Value::Blob { value } => Value::Blob(value.as_ref().into()),
    }
}

fn proto_value_from_value(value: Value) -> proto::Value {
    match value {
        Value::Null => proto::Value::Null,
        Value::Integer(value) => proto::Value::Integer { value },
        Value::Real(value) => proto::Value::Float { value },
        Value::Text(value) => proto::Value::Text {
            value: value.into(),
        },
        Value::Blob(value) => proto::Value::Blob {
            value: value.into(),
        },
    }
}

fn proto_describe_result_from_describe_response(
    response: DescribeResponse,
) -> proto::DescribeResult {
    proto::DescribeResult {
        params: response
            .params
            .into_iter()
            .map(|p| proto::DescribeParam { name: p.name })
            .collect(),
        cols: response
            .cols
            .into_iter()
            .map(|c| proto::DescribeCol {
                name: c.name,
                decltype: c.decltype,
            })
            .collect(),
        is_explain: response.is_explain,
        is_readonly: response.is_readonly,
    }
}

pub fn stmt_error_from_sqld_error(sqld_error: SqldError) -> Result<StmtError, SqldError> {
    Ok(match sqld_error {
        SqldError::LibSqlInvalidQueryParams(source) => StmtError::ArgsInvalid { source },
        SqldError::LibSqlTxTimeout(_) => StmtError::TransactionTimeout,
        SqldError::LibSqlTxBusy => StmtError::TransactionBusy,
        SqldError::RusqliteError(rusqlite_error) => match rusqlite_error {
            rusqlite::Error::SqliteFailure(sqlite_error, Some(message)) => StmtError::SqliteError {
                source: sqlite_error,
                message,
            },
            rusqlite::Error::SqliteFailure(sqlite_error, None) => StmtError::SqliteError {
                message: sqlite_error.to_string(),
                source: sqlite_error,
            },
            rusqlite::Error::SqlInputError {
                error: sqlite_error,
                msg: message,
                offset,
                ..
            } => StmtError::SqlInputError {
                source: sqlite_error,
                message,
                offset,
            },
            rusqlite_error => return Err(SqldError::RusqliteError(rusqlite_error)),
        },
        sqld_error => return Err(sqld_error),
    })
}

pub fn proto_error_from_stmt_error(error: &StmtError) -> hrana::proto::Error {
    hrana::proto::Error {
        message: error.to_string(),
        code: error.code().into(),
    }
}

impl StmtError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::SqlParse { .. } => "SQL_PARSE_ERROR",
            Self::SqlNoStmt => "SQL_NO_STATEMENT",
            Self::SqlManyStmts => "SQL_MANY_STATEMENTS",
            Self::ArgsInvalid { .. } => "ARGS_INVALID",
            Self::ArgsBothPositionalAndNamed => "ARGS_BOTH_POSITIONAL_AND_NAMED",
            Self::TransactionTimeout => "TRANSACTION_TIMEOUT",
            Self::TransactionBusy => "TRANSACTION_BUSY",
            Self::SqliteError { source, .. } => sqlite_error_code(source.code),
            Self::SqlInputError { .. } => "SQL_INPUT_ERROR",
        }
    }
}

fn sqlite_error_code(code: rusqlite::ffi::ErrorCode) -> &'static str {
    match code {
        rusqlite::ErrorCode::InternalMalfunction => "SQLITE_INTERNAL",
        rusqlite::ErrorCode::PermissionDenied => "SQLITE_PERM",
        rusqlite::ErrorCode::OperationAborted => "SQLITE_ABORT",
        rusqlite::ErrorCode::DatabaseBusy => "SQLITE_BUSY",
        rusqlite::ErrorCode::DatabaseLocked => "SQLITE_LOCKED",
        rusqlite::ErrorCode::OutOfMemory => "SQLITE_NOMEM",
        rusqlite::ErrorCode::ReadOnly => "SQLITE_READONLY",
        rusqlite::ErrorCode::OperationInterrupted => "SQLITE_INTERRUPT",
        rusqlite::ErrorCode::SystemIoFailure => "SQLITE_IOERR",
        rusqlite::ErrorCode::DatabaseCorrupt => "SQLITE_CORRUPT",
        rusqlite::ErrorCode::NotFound => "SQLITE_NOTFOUND",
        rusqlite::ErrorCode::DiskFull => "SQLITE_FULL",
        rusqlite::ErrorCode::CannotOpen => "SQLITE_CANTOPEN",
        rusqlite::ErrorCode::FileLockingProtocolFailed => "SQLITE_PROTOCOL",
        rusqlite::ErrorCode::SchemaChanged => "SQLITE_SCHEMA",
        rusqlite::ErrorCode::TooBig => "SQLITE_TOOBIG",
        rusqlite::ErrorCode::ConstraintViolation => "SQLITE_CONSTRAINT",
        rusqlite::ErrorCode::TypeMismatch => "SQLITE_MISMATCH",
        rusqlite::ErrorCode::ApiMisuse => "SQLITE_MISUSE",
        rusqlite::ErrorCode::NoLargeFileSupport => "SQLITE_NOLFS",
        rusqlite::ErrorCode::AuthorizationForStatementDenied => "SQLITE_AUTH",
        rusqlite::ErrorCode::ParameterOutOfRange => "SQLITE_RANGE",
        rusqlite::ErrorCode::NotADatabase => "SQLITE_NOTADB",
        rusqlite::ErrorCode::Unknown => "SQLITE_UNKNOWN",
        _ => "SQLITE_UNKNOWN",
    }
}

impl From<&proto::Value> for Value {
    fn from(proto_value: &proto::Value) -> Value {
        proto_value_to_value(proto_value)
    }
}

impl From<Value> for proto::Value {
    fn from(value: Value) -> proto::Value {
        proto_value_from_value(value)
    }
}
