use anyhow::Result;
use mysql_async::{consts::ColumnType, from_value_opt, prelude::*, Opts, OptsBuilder, SslOpts};
use spin_core::wasmtime::component::Resource;
use spin_core::{async_trait, HostComponent};
use spin_world::v1::mysql as v1;
use spin_world::v1::rdbms_types::{Column, DbDataType, DbValue, ParameterValue, RowSet};
use spin_world::v2::mysql::{self as v2, Connection};
use std::sync::Arc;
use url::Url;

/// A simple implementation to support outbound mysql connection
#[derive(Default)]
pub struct OutboundMysql {
    pub connections: table::Table<mysql_async::Conn>,
}

impl OutboundMysql {
    async fn get_conn(
        &mut self,
        connection: Resource<Connection>,
    ) -> Result<&mut mysql_async::Conn, v2::Error> {
        self.connections
            .get_mut(connection.rep())
            .ok_or_else(|| v2::Error::ConnectionFailed("no connection found".into()))
    }
}

impl HostComponent for OutboundMysql {
    type Data = Self;

    fn add_to_linker<T: Send>(
        linker: &mut spin_core::Linker<T>,
        get: impl Fn(&mut spin_core::Data<T>) -> &mut Self::Data + Send + Sync + Copy + 'static,
    ) -> anyhow::Result<()> {
        v2::add_to_linker(linker, get)?;
        v1::add_to_linker(linker, get)
    }

    fn build_data(&self) -> Self::Data {
        Default::default()
    }
}

impl v2::Host for OutboundMysql {}

#[async_trait]
impl v2::HostConnection for OutboundMysql {
    async fn open(&mut self, address: String) -> Result<Result<Resource<Connection>, v2::Error>> {
        Ok(async {
            self.connections
                .push(
                    build_conn(&address)
                        .await
                        .map_err(|e| v2::Error::ConnectionFailed(format!("{e:?}")))?,
                )
                .map_err(|_| v2::Error::ConnectionFailed("too many connections".into()))
                .map(Resource::new_own)
        }
        .await)
    }

    async fn execute(
        &mut self,
        connection: Resource<Connection>,
        statement: String,
        params: Vec<ParameterValue>,
    ) -> Result<Result<(), v2::Error>> {
        Ok(async {
            let db_params = params.into_iter().map(to_sql_parameter).collect::<Vec<_>>();
            let parameters = mysql_async::Params::Positional(db_params);

            self.get_conn(connection)
                .await?
                .exec_batch(&statement, &[parameters])
                .await
                .map_err(|e| v2::Error::QueryFailed(format!("{:?}", e)))?;

            Ok(())
        }
        .await)
    }

    async fn query(
        &mut self,
        connection: Resource<Connection>,
        statement: String,
        params: Vec<ParameterValue>,
    ) -> Result<Result<RowSet, v2::Error>> {
        Ok(async {
            let db_params = params.into_iter().map(to_sql_parameter).collect::<Vec<_>>();
            let parameters = mysql_async::Params::Positional(db_params);

            let mut query_result = self
                .get_conn(connection)
                .await?
                .exec_iter(&statement, parameters)
                .await
                .map_err(|e| v2::Error::QueryFailed(format!("{:?}", e)))?;

            // We have to get these before collect() destroys them
            let columns = convert_columns(query_result.columns());

            match query_result.collect::<mysql_async::Row>().await {
                Err(e) => Err(v2::Error::Other(e.to_string())),
                Ok(result_set) => {
                    let rows = result_set
                        .into_iter()
                        .map(|row| convert_row(row, &columns))
                        .collect::<Result<Vec<_>, _>>()?;

                    Ok(RowSet { columns, rows })
                }
            }
        }
        .await)
    }

    fn drop(&mut self, connection: Resource<Connection>) -> Result<()> {
        self.connections.remove(connection.rep());
        Ok(())
    }
}

/// Delegate a function call to the v2::HostConnection implementation
macro_rules! delegate {
    ($self:ident.$name:ident($address:expr, $($arg:expr),*)) => {{
        let connection = match <Self as v2::HostConnection>::open($self, $address).await? {
            Ok(c) => c,
            Err(e) => return Ok(Err(to_legacy_error(e))),
        };
        Ok(<Self as v2::HostConnection>::$name($self, connection, $($arg),*)
            .await?
            .map_err(|e| to_legacy_error(e)))
    }};
}

#[async_trait]
impl v1::Host for OutboundMysql {
    async fn execute(
        &mut self,
        address: String,
        statement: String,
        params: Vec<ParameterValue>,
    ) -> Result<Result<(), v1::MysqlError>> {
        delegate!(self.execute(address, statement, params))
    }

    async fn query(
        &mut self,
        address: String,
        statement: String,
        params: Vec<ParameterValue>,
    ) -> Result<Result<RowSet, v1::MysqlError>> {
        delegate!(self.query(address, statement, params))
    }
}

fn to_legacy_error(error: v2::Error) -> v1::MysqlError {
    match error {
        v2::Error::ConnectionFailed(e) => v1::MysqlError::ConnectionFailed(e),
        v2::Error::BadParameter(e) => v1::MysqlError::BadParameter(e),
        v2::Error::QueryFailed(e) => v1::MysqlError::QueryFailed(e),
        v2::Error::ValueConversionFailed(e) => v1::MysqlError::ValueConversionFailed(e),
        v2::Error::Other(e) => v1::MysqlError::OtherError(e),
    }
}

fn to_sql_parameter(value: ParameterValue) -> mysql_async::Value {
    match value {
        ParameterValue::Boolean(v) => mysql_async::Value::from(v),
        ParameterValue::Int32(v) => mysql_async::Value::from(v),
        ParameterValue::Int64(v) => mysql_async::Value::from(v),
        ParameterValue::Int8(v) => mysql_async::Value::from(v),
        ParameterValue::Int16(v) => mysql_async::Value::from(v),
        ParameterValue::Floating32(v) => mysql_async::Value::from(v),
        ParameterValue::Floating64(v) => mysql_async::Value::from(v),
        ParameterValue::Uint8(v) => mysql_async::Value::from(v),
        ParameterValue::Uint16(v) => mysql_async::Value::from(v),
        ParameterValue::Uint32(v) => mysql_async::Value::from(v),
        ParameterValue::Uint64(v) => mysql_async::Value::from(v),
        ParameterValue::Str(v) => mysql_async::Value::from(v),
        ParameterValue::Binary(v) => mysql_async::Value::from(v),
        ParameterValue::DbNull => mysql_async::Value::NULL,
    }
}

fn convert_columns(columns: Option<Arc<[mysql_async::Column]>>) -> Vec<Column> {
    match columns {
        Some(columns) => columns.iter().map(convert_column).collect(),
        None => vec![],
    }
}

fn convert_column(column: &mysql_async::Column) -> Column {
    let name = column.name_str().into_owned();
    let data_type = convert_data_type(column);

    Column { name, data_type }
}

fn convert_data_type(column: &mysql_async::Column) -> DbDataType {
    let column_type = column.column_type();

    if column_type.is_numeric_type() {
        convert_numeric_type(column)
    } else if column_type.is_character_type() {
        convert_character_type(column)
    } else {
        DbDataType::Other
    }
}

fn convert_character_type(column: &mysql_async::Column) -> DbDataType {
    match (column.column_type(), is_binary(column)) {
        (ColumnType::MYSQL_TYPE_BLOB, false) => DbDataType::Str, // TEXT type
        (ColumnType::MYSQL_TYPE_BLOB, _) => DbDataType::Binary,
        (ColumnType::MYSQL_TYPE_LONG_BLOB, _) => DbDataType::Binary,
        (ColumnType::MYSQL_TYPE_MEDIUM_BLOB, _) => DbDataType::Binary,
        (ColumnType::MYSQL_TYPE_STRING, true) => DbDataType::Binary, // BINARY type
        (ColumnType::MYSQL_TYPE_STRING, _) => DbDataType::Str,
        (ColumnType::MYSQL_TYPE_VAR_STRING, true) => DbDataType::Binary, // VARBINARY type
        (ColumnType::MYSQL_TYPE_VAR_STRING, _) => DbDataType::Str,
        (_, _) => DbDataType::Other,
    }
}

fn convert_numeric_type(column: &mysql_async::Column) -> DbDataType {
    match (column.column_type(), is_signed(column)) {
        (ColumnType::MYSQL_TYPE_DOUBLE, _) => DbDataType::Floating64,
        (ColumnType::MYSQL_TYPE_FLOAT, _) => DbDataType::Floating32,
        (ColumnType::MYSQL_TYPE_INT24, true) => DbDataType::Int32,
        (ColumnType::MYSQL_TYPE_INT24, false) => DbDataType::Uint32,
        (ColumnType::MYSQL_TYPE_LONG, true) => DbDataType::Int32,
        (ColumnType::MYSQL_TYPE_LONG, false) => DbDataType::Uint32,
        (ColumnType::MYSQL_TYPE_LONGLONG, true) => DbDataType::Int64,
        (ColumnType::MYSQL_TYPE_LONGLONG, false) => DbDataType::Uint64,
        (ColumnType::MYSQL_TYPE_SHORT, true) => DbDataType::Int16,
        (ColumnType::MYSQL_TYPE_SHORT, false) => DbDataType::Uint16,
        (ColumnType::MYSQL_TYPE_TINY, true) => DbDataType::Int8,
        (ColumnType::MYSQL_TYPE_TINY, false) => DbDataType::Uint8,
        (_, _) => DbDataType::Other,
    }
}

fn is_signed(column: &mysql_async::Column) -> bool {
    !column
        .flags()
        .contains(mysql_async::consts::ColumnFlags::UNSIGNED_FLAG)
}

fn is_binary(column: &mysql_async::Column) -> bool {
    column
        .flags()
        .contains(mysql_async::consts::ColumnFlags::BINARY_FLAG)
}

fn convert_row(mut row: mysql_async::Row, columns: &[Column]) -> Result<Vec<DbValue>, v2::Error> {
    let mut result = Vec::with_capacity(row.len());
    for index in 0..row.len() {
        result.push(convert_entry(&mut row, index, columns)?);
    }
    Ok(result)
}

fn convert_entry(
    row: &mut mysql_async::Row,
    index: usize,
    columns: &[Column],
) -> Result<DbValue, v2::Error> {
    match (row.take(index), columns.get(index)) {
        (None, _) => Ok(DbValue::DbNull), // TODO: is this right or is this an "index out of range" thing
        (_, None) => Err(v2::Error::Other(format!(
            "Can't get column at index {}",
            index
        ))),
        (Some(mysql_async::Value::NULL), _) => Ok(DbValue::DbNull),
        (Some(value), Some(column)) => convert_value(value, column),
    }
}

fn convert_value(value: mysql_async::Value, column: &Column) -> Result<DbValue, v2::Error> {
    match column.data_type {
        DbDataType::Binary => convert_value_to::<Vec<u8>>(value).map(DbValue::Binary),
        DbDataType::Boolean => convert_value_to::<bool>(value).map(DbValue::Boolean),
        DbDataType::Floating32 => convert_value_to::<f32>(value).map(DbValue::Floating32),
        DbDataType::Floating64 => convert_value_to::<f64>(value).map(DbValue::Floating64),
        DbDataType::Int8 => convert_value_to::<i8>(value).map(DbValue::Int8),
        DbDataType::Int16 => convert_value_to::<i16>(value).map(DbValue::Int16),
        DbDataType::Int32 => convert_value_to::<i32>(value).map(DbValue::Int32),
        DbDataType::Int64 => convert_value_to::<i64>(value).map(DbValue::Int64),
        DbDataType::Str => convert_value_to::<String>(value).map(DbValue::Str),
        DbDataType::Uint8 => convert_value_to::<u8>(value).map(DbValue::Uint8),
        DbDataType::Uint16 => convert_value_to::<u16>(value).map(DbValue::Uint16),
        DbDataType::Uint32 => convert_value_to::<u32>(value).map(DbValue::Uint32),
        DbDataType::Uint64 => convert_value_to::<u64>(value).map(DbValue::Uint64),
        DbDataType::Other => Err(v2::Error::ValueConversionFailed(format!(
            "Cannot convert value {:?} in column {} data type {:?}",
            value, column.name, column.data_type
        ))),
    }
}

async fn build_conn(address: &str) -> Result<mysql_async::Conn, mysql_async::Error> {
    tracing::log::debug!("Build new connection: {}", address);

    let opts = build_opts(address)?;

    let connection_pool = mysql_async::Pool::new(opts);

    connection_pool.get_conn().await
}

fn is_ssl_param(s: &str) -> bool {
    ["ssl-mode", "sslmode"].contains(&s.to_lowercase().as_str())
}

/// The mysql_async crate blows up if you pass it an SSL parameter and doesn't support SSL opts properly. This function
/// is a workaround to manually set SSL opts if the user requests them.
///
/// We only support ssl-mode in the query as per
/// https://dev.mysql.com/doc/connector-j/8.0/en/connector-j-connp-props-security.html#cj-conn-prop_sslMode.
///
/// An issue has been filed in the upstream repository https://github.com/blackbeam/mysql_async/issues/225.
fn build_opts(address: &str) -> Result<Opts, mysql_async::Error> {
    let url = Url::parse(address)?;

    let use_ssl = url
        .query_pairs()
        .any(|(k, v)| is_ssl_param(&k) && v.to_lowercase() != "disabled");

    let query_without_ssl: Vec<(_, _)> = url
        .query_pairs()
        .filter(|(k, _v)| !is_ssl_param(k))
        .collect();
    let mut cleaned_url = url.clone();
    cleaned_url.set_query(None);
    cleaned_url
        .query_pairs_mut()
        .extend_pairs(query_without_ssl);

    Ok(OptsBuilder::from_opts(cleaned_url.as_str())
        .ssl_opts(if use_ssl {
            Some(SslOpts::default())
        } else {
            None
        })
        .into())
}

fn convert_value_to<T: FromValue>(value: mysql_async::Value) -> Result<T, v2::Error> {
    from_value_opt::<T>(value).map_err(|e| v2::Error::ValueConversionFailed(format!("{}", e)))
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_mysql_address_without_ssl_mode() {
        assert!(build_opts("mysql://myuser:password@127.0.0.1/db")
            .unwrap()
            .ssl_opts()
            .is_none())
    }

    #[test]
    fn test_mysql_address_with_ssl_mode_disabled() {
        assert!(
            build_opts("mysql://myuser:password@127.0.0.1/db?ssl-mode=DISABLED")
                .unwrap()
                .ssl_opts()
                .is_none()
        )
    }

    #[test]
    fn test_mysql_address_with_ssl_mode_verify_ca() {
        assert!(
            build_opts("mysql://myuser:password@127.0.0.1/db?sslMode=VERIFY_CA")
                .unwrap()
                .ssl_opts()
                .is_some()
        )
    }

    #[test]
    fn test_mysql_address_with_more_to_query() {
        let address = "mysql://myuser:password@127.0.0.1/db?SsLmOdE=VERIFY_CA&pool_max=10";
        assert!(build_opts(address).unwrap().ssl_opts().is_some());
        assert_eq!(
            build_opts(address).unwrap().pool_opts().constraints().max(),
            10
        )
    }
}
