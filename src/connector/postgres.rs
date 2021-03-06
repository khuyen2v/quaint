mod conversion;
mod error;

use crate::{
    ast::{ParameterizedValue, Query},
    connector::{metrics, queryable::*, ResultSet, DBIO},
    error::{Error, ErrorKind},
    visitor::{self, Visitor},
};
use futures::{future::FutureExt, lock::Mutex};
use native_tls::{Certificate, Identity, TlsConnector};
use percent_encoding::percent_decode;
use postgres_native_tls::MakeTlsConnector;
use std::{
    borrow::{Borrow, Cow},
    fs,
    future::Future,
    time::Duration,
};
use tokio::time::timeout;
use tokio_postgres::{config::SslMode, Client, Config};
use url::Url;

pub(crate) const DEFAULT_SCHEMA: &str = "public";

#[derive(Clone)]
struct Hidden<T>(T);

impl<T> std::fmt::Debug for Hidden<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<HIDDEN>")
    }
}

struct PostgresClient(Mutex<Client>);

impl std::fmt::Debug for PostgresClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PostgresClient")
    }
}

/// A connector interface for the PostgreSQL database.
#[derive(Debug)]
pub struct PostgreSql {
    client: PostgresClient,
    socket_timeout: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SslAcceptMode {
    Strict,
    AcceptInvalidCerts,
}

#[derive(Debug, Clone)]
pub struct SslParams {
    certificate_file: Option<String>,
    identity_file: Option<String>,
    identity_password: Hidden<Option<String>>,
    ssl_accept_mode: SslAcceptMode,
}

#[derive(Debug)]
struct SslAuth {
    certificate: Hidden<Option<Certificate>>,
    identity: Hidden<Option<Identity>>,
    ssl_accept_mode: SslAcceptMode,
}

impl Default for SslAuth {
    fn default() -> Self {
        Self {
            certificate: Hidden(None),
            identity: Hidden(None),
            ssl_accept_mode: SslAcceptMode::AcceptInvalidCerts,
        }
    }
}

impl SslAuth {
    fn certificate(&mut self, certificate: Certificate) -> &mut Self {
        self.certificate = Hidden(Some(certificate));
        self
    }

    fn identity(&mut self, identity: Identity) -> &mut Self {
        self.identity = Hidden(Some(identity));
        self
    }

    fn accept_mode(&mut self, mode: SslAcceptMode) -> &mut Self {
        self.ssl_accept_mode = mode;
        self
    }
}

impl SslParams {
    async fn into_auth(self) -> crate::Result<SslAuth> {
        let mut auth = SslAuth::default();
        auth.accept_mode(self.ssl_accept_mode);

        if let Some(ref cert_file) = self.certificate_file {
            let cert = fs::read(cert_file).map_err(|err| {
                Error::builder(ErrorKind::TlsError {
                    message: format!("cert file not found ({})", err),
                })
                .build()
            })?;

            auth.certificate(Certificate::from_pem(&cert)?);
        }

        if let Some(ref identity_file) = self.identity_file {
            let db = fs::read(identity_file).map_err(|err| {
                Error::builder(ErrorKind::TlsError {
                    message: format!("identity file not found ({})", err),
                })
                .build()
            })?;
            let password = self.identity_password.0.as_ref().map(|s| s.as_str()).unwrap_or("");
            let identity = Identity::from_pkcs12(&db, &password)?;

            auth.identity(identity);
        }

        Ok(auth)
    }
}

/// Wraps a connection url and exposes the parsing logic used by quaint, including default values.
#[derive(Debug, Clone)]
pub struct PostgresUrl {
    url: Url,
    query_params: PostgresUrlQueryParams,
}

impl PostgresUrl {
    /// Parse `Url` to `PostgresUrl`. Returns error for mistyped connection
    /// parameters.
    pub fn new(url: Url) -> Result<Self, Error> {
        let query_params = Self::parse_query_params(&url)?;

        Ok(Self { url, query_params })
    }

    /// The bare `Url` to the database.
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// The percent-decoded database username.
    pub fn username(&self) -> Cow<str> {
        match percent_decode(self.url.username().as_bytes()).decode_utf8() {
            Ok(username) => username,
            Err(_) => {
                #[cfg(not(feature = "tracing-log"))]
                warn!("Couldn't decode username to UTF-8, using the non-decoded version.");
                #[cfg(feature = "tracing-log")]
                tracing::warn!("Couldn't decode username to UTF-8, using the non-decoded version.");

                self.url.username().into()
            }
        }
    }

    /// The database host. Taken first from the `host` query parameter, then
    /// from the `host` part of the URL. For socket connections, the query
    /// parameter must be used.
    ///
    /// If none of them are set, defaults to `localhost`.
    pub fn host(&self) -> &str {
        match (self.query_params.host.as_ref(), self.url.host_str()) {
            (Some(host), _) => host.as_str(),
            (None, Some("")) => "localhost",
            (None, None) => "localhost",
            (None, Some(host)) => host,
        }
    }

    /// Name of the database connected. Defaults to `postgres`.
    pub fn dbname(&self) -> &str {
        match self.url.path_segments() {
            Some(mut segments) => segments.next().unwrap_or("postgres"),
            None => "postgres",
        }
    }

    /// The percent-decoded database password.
    pub fn password(&self) -> Cow<str> {
        match self
            .url
            .password()
            .and_then(|pw| percent_decode(pw.as_bytes()).decode_utf8().ok())
        {
            Some(password) => password,
            None => self.url.password().unwrap_or("").into(),
        }
    }

    /// The database port, defaults to `5432`.
    pub fn port(&self) -> u16 {
        self.url.port().unwrap_or(5432)
    }

    /// The database schema, defaults to `public`.
    pub fn schema(&self) -> &str {
        &self.query_params.schema
    }

    fn default_connection_limit() -> usize {
        num_cpus::get_physical() * 2 + 1
    }

    fn parse_query_params(url: &Url) -> Result<PostgresUrlQueryParams, Error> {
        let mut connection_limit = Self::default_connection_limit();
        let mut schema = String::from(DEFAULT_SCHEMA);
        let mut certificate_file = None;
        let mut identity_file = None;
        let mut identity_password = None;
        let mut ssl_accept_mode = SslAcceptMode::AcceptInvalidCerts;
        let mut ssl_mode = SslMode::Prefer;
        let mut host = None;
        let mut socket_timeout = None;
        let mut connect_timeout = Duration::from_secs(5);

        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "sslmode" => {
                    match v.as_ref() {
                        "disable" => ssl_mode = SslMode::Disable,
                        "prefer" => ssl_mode = SslMode::Prefer,
                        "require" => ssl_mode = SslMode::Require,
                        _ => {
                            #[cfg(not(feature = "tracing-log"))]
                            debug!("Unsupported ssl mode {}, defaulting to 'prefer'", v);
                            #[cfg(feature = "tracing-log")]
                            tracing::debug!(message = "Unsupported SSL mode, defaulting to `prefer`", mode = &*v);
                        }
                    };
                }
                "sslcert" => {
                    certificate_file = Some(v.to_string());
                }
                "sslidentity" => {
                    identity_file = Some(v.to_string());
                }
                "sslpassword" => {
                    identity_password = Some(v.to_string());
                }
                "sslaccept" => {
                    match v.as_ref() {
                        "strict" => {
                            ssl_accept_mode = SslAcceptMode::Strict;
                        }
                        "accept_invalid_certs" => {
                            ssl_accept_mode = SslAcceptMode::AcceptInvalidCerts;
                        }
                        _ => {
                            #[cfg(not(feature = "tracing-log"))]
                            debug!("Unsupported SSL accept mode {}, defaulting to `strict`", v);
                            #[cfg(feature = "tracing-log")]
                            tracing::debug!(
                                message = "Unsupported SSL accept mode, defaulting to `strict`",
                                mode = &*v
                            );

                            ssl_accept_mode = SslAcceptMode::Strict;
                        }
                    };
                }
                "schema" => {
                    schema = v.to_string();
                }
                "connection_limit" => {
                    let as_int: usize = v
                        .parse()
                        .map_err(|_| Error::builder(ErrorKind::InvalidConnectionArguments).build())?;
                    connection_limit = as_int;
                }
                "host" => {
                    host = Some(v.to_string());
                }
                "socket_timeout" => {
                    let as_int = v
                        .parse()
                        .map_err(|_| Error::builder(ErrorKind::InvalidConnectionArguments).build())?;
                    socket_timeout = Some(Duration::from_secs(as_int));
                }
                "connect_timeout" => {
                    let as_int = v
                        .parse()
                        .map_err(|_| Error::builder(ErrorKind::InvalidConnectionArguments).build())?;
                    connect_timeout = Duration::from_secs(as_int);
                }
                _ => {
                    #[cfg(not(feature = "tracing-log"))]
                    trace!("Discarding connection string param: {}", k);
                    #[cfg(feature = "tracing-log")]
                    tracing::trace!(message = "Discarding connection string param", param = &*k);
                }
            };
        }

        Ok(PostgresUrlQueryParams {
            ssl_params: SslParams {
                certificate_file,
                identity_file,
                ssl_accept_mode,
                identity_password: Hidden(identity_password),
            },
            connection_limit,
            schema,
            ssl_mode,
            host,
            connect_timeout,
            socket_timeout,
        })
    }

    pub(crate) fn ssl_params(&self) -> &SslParams {
        &self.query_params.ssl_params
    }

    #[cfg(feature = "pooled")]
    pub(crate) fn connection_limit(&self) -> usize {
        self.query_params.connection_limit
    }

    pub(crate) fn to_config(&self) -> Config {
        let mut config = Config::new();

        config.user(self.username().borrow());
        config.password(self.password().borrow() as &str);
        config.host(self.host());
        config.port(self.port());
        config.dbname(self.dbname());
        config.connect_timeout(self.query_params.connect_timeout);

        config.ssl_mode(self.query_params.ssl_mode);

        config
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PostgresUrlQueryParams {
    ssl_params: SslParams,
    connection_limit: usize,
    schema: String,
    ssl_mode: SslMode,
    host: Option<String>,
    socket_timeout: Option<Duration>,
    connect_timeout: Duration,
}

impl PostgreSql {
    /// Create a new connection to the database.
    pub async fn new(url: PostgresUrl) -> crate::Result<Self> {
        let config = url.to_config();

        let mut tls_builder = TlsConnector::builder();

        {
            let ssl_params = url.ssl_params();
            let auth = ssl_params.to_owned().into_auth().await?;

            if let Some(certificate) = auth.certificate.0 {
                tls_builder.add_root_certificate(certificate);
            }

            tls_builder.danger_accept_invalid_certs(auth.ssl_accept_mode == SslAcceptMode::AcceptInvalidCerts);

            if let Some(identity) = auth.identity.0 {
                tls_builder.identity(identity);
            }
        }

        let tls = MakeTlsConnector::new(tls_builder.build()?);
        let (client, conn) = config.connect(tls).await?;
        tokio::spawn(conn.map(|r| r.unwrap()));

        let schema = url.schema();
        let path = format!("SET search_path = \"{}\"", schema);

        client.simple_query(path.as_str()).await?;

        Ok(Self {
            client: PostgresClient(Mutex::new(client)),
            socket_timeout: url.query_params.socket_timeout,
        })
    }

    async fn timeout<T, F, E>(&self, f: F) -> crate::Result<T>
    where
        F: Future<Output = std::result::Result<T, E>>,
        E: Into<Error>,
    {
        match self.socket_timeout {
            Some(duration) => match timeout(duration, f).await {
                Ok(Ok(result)) => Ok(result),
                Ok(Err(err)) => Err(err.into()),
                Err(to) => Err(to.into()),
            },
            None => match f.await {
                Ok(result) => Ok(result),
                Err(err) => Err(err.into()),
            },
        }
    }
}

impl TransactionCapable for PostgreSql {}

impl Queryable for PostgreSql {
    fn query<'a>(&'a self, q: Query<'a>) -> DBIO<'a, ResultSet> {
        let (sql, params) = visitor::Postgres::build(q);
        DBIO::new(async move { self.query_raw(sql.as_str(), &params[..]).await })
    }

    fn execute<'a>(&'a self, q: Query<'a>) -> DBIO<'a, u64> {
        let (sql, params) = visitor::Postgres::build(q);
        DBIO::new(async move { self.execute_raw(sql.as_str(), &params[..]).await })
    }

    fn query_raw<'a>(&'a self, sql: &'a str, params: &'a [ParameterizedValue<'a>]) -> DBIO<'a, ResultSet> {
        metrics::query("postgres.query_raw", sql, params, move || async move {
            let client = self.client.0.lock().await;
            let stmt = self.timeout(client.prepare(sql)).await?;

            let rows = self
                .timeout(client.query(&stmt, conversion::conv_params(params).as_slice()))
                .await?;
            let mut result = ResultSet::new(stmt.to_column_names(), Vec::new());

            for row in rows {
                result.rows.push(row.get_result_row()?);
            }

            Ok(result)
        })
    }

    fn execute_raw<'a>(&'a self, sql: &'a str, params: &'a [ParameterizedValue<'a>]) -> DBIO<'a, u64> {
        metrics::query("postgres.execute_raw", sql, params, move || async move {
            let client = self.client.0.lock().await;
            let stmt = self.timeout(client.prepare(sql)).await?;

            let changes = self
                .timeout(client.execute(&stmt, conversion::conv_params(params).as_slice()))
                .await?;

            Ok(changes)
        })
    }

    fn raw_cmd<'a>(&'a self, cmd: &'a str) -> DBIO<'a, ()> {
        metrics::query("postgres.raw_cmd", cmd, &[], move || async move {
            let client = self.client.0.lock().await;
            self.timeout(client.simple_query(cmd)).await?;

            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{connector::Queryable, error::*, single::Quaint};
    use once_cell::sync::Lazy;
    use std::env;
    use url::Url;

    static CONN_STR: Lazy<String> = Lazy::new(|| env::var("TEST_PSQL").expect("TEST_PSQL env var"));

    #[test]
    fn should_parse_socket_url() {
        let url = PostgresUrl::new(Url::parse("postgresql:///dbname?host=/var/run/psql.sock").unwrap()).unwrap();
        assert_eq!("dbname", url.dbname());
        assert_eq!("/var/run/psql.sock", url.host());
    }

    #[test]
    fn should_parse_escaped_url() {
        let url = PostgresUrl::new(Url::parse("postgresql:///dbname?host=%2Fvar%2Frun%2Fpostgresql").unwrap()).unwrap();
        assert_eq!("dbname", url.dbname());
        assert_eq!("/var/run/postgresql", url.host());
    }

    #[test]
    fn should_parse_default_host() {
        let url = PostgresUrl::new(Url::parse("postgresql:///dbname").unwrap()).unwrap();
        assert_eq!("dbname", url.dbname());
        assert_eq!("localhost", url.host());
    }

    #[tokio::test]
    async fn should_provide_a_database_connection() {
        let connection = Quaint::new(&CONN_STR).await.unwrap();

        let res = connection
            .query_raw("select * from \"pg_catalog\".\"pg_am\" where amtype = 'x'", &[])
            .await
            .unwrap();

        // No results expected.
        assert!(res.is_empty());
    }

    #[allow(unused)]
    const TABLE_DEF: &str = r#"
    CREATE TABLE "user"(
        id       int4    PRIMARY KEY     NOT NULL,
        name     text    NOT NULL,
        age      int4    NOT NULL,
        salary   float4
    );
    "#;

    #[allow(unused)]
    const CREATE_USER: &str = r#"
    INSERT INTO "user" (id, name, age, salary)
    VALUES (1, 'Joe', 27, 20000.00 );
    "#;

    #[allow(unused)]
    const DROP_TABLE: &str = "DROP TABLE IF EXISTS \"user\";";

    #[tokio::test]
    async fn should_map_columns_correctly() {
        let connection = Quaint::new(&CONN_STR).await.unwrap();

        connection.query_raw(DROP_TABLE, &[]).await.unwrap();
        connection.query_raw(TABLE_DEF, &[]).await.unwrap();

        let changes = connection.execute_raw(CREATE_USER, &[]).await.unwrap();
        assert_eq!(1, changes);

        let rows = connection.query_raw("SELECT * FROM \"user\"", &[]).await.unwrap();
        assert_eq!(rows.len(), 1);

        let row = rows.get(0).unwrap();
        assert_eq!(row["id"].as_i64(), Some(1));
        assert_eq!(row["name"].as_str(), Some("Joe"));
        assert_eq!(row["age"].as_i64(), Some(27));

        assert_eq!(row["salary"].as_f64(), Some(20000.0));
    }

    #[tokio::test]
    async fn test_uniq_constraint_violation() {
        let conn = Quaint::new(&CONN_STR).await.unwrap();

        let _ = conn.raw_cmd("DROP TABLE test_uniq_constraint_violation").await;
        let _ = conn.raw_cmd("DROP INDEX idx_uniq_constraint_violation").await;

        conn.raw_cmd("CREATE TABLE test_uniq_constraint_violation (id1 int, id2 int)")
            .await
            .unwrap();
        conn.raw_cmd("CREATE UNIQUE INDEX idx_uniq_constraint_violation ON test_uniq_constraint_violation (id1, id2)")
            .await
            .unwrap();

        conn.query_raw(
            "INSERT INTO test_uniq_constraint_violation (id1, id2) VALUES (1, 2)",
            &[],
        )
        .await
        .unwrap();

        let res = conn
            .query_raw(
                "INSERT INTO test_uniq_constraint_violation (id1, id2) VALUES (1, 2)",
                &[],
            )
            .await;

        let err = res.unwrap_err();

        match err.kind() {
            ErrorKind::UniqueConstraintViolation { constraint } => {
                assert_eq!(Some("23505"), err.original_code());
                assert_eq!(Some("Key (id1, id2)=(1, 2) already exists."), err.original_message());

                assert_eq!(
                    &DatabaseConstraint::Fields(vec![String::from("id1"), String::from("id2")]),
                    constraint,
                )
            }
            _ => panic!(err),
        }
    }

    #[tokio::test]
    async fn test_null_constraint_violation() {
        let conn = Quaint::new(&CONN_STR).await.unwrap();

        let _ = conn.raw_cmd("DROP TABLE test_null_constraint_violation").await;

        conn.raw_cmd("CREATE TABLE test_null_constraint_violation (id1 int not null, id2 int not null)")
            .await
            .unwrap();

        let res = conn
            .query_raw("INSERT INTO test_null_constraint_violation DEFAULT VALUES", &[])
            .await;

        let err = res.unwrap_err();

        match err.kind() {
            ErrorKind::NullConstraintViolation { constraint } => {
                assert_eq!(Some("23502"), err.original_code());
                assert_eq!(
                    Some("null value in column \"id1\" violates not-null constraint"),
                    err.original_message()
                );
                assert_eq!(&DatabaseConstraint::Fields(vec![String::from("id1")]), constraint)
            }
            _ => panic!(err),
        }
    }

    #[tokio::test]
    async fn test_custom_search_path() {
        let mut url = Url::parse(&CONN_STR).unwrap();
        url.query_pairs_mut().append_pair("schema", "musti-test");

        let client = Quaint::new(url.as_str()).await.unwrap();

        let result_set = client.query_raw("SHOW search_path", &[]).await.unwrap();
        let row = result_set.first().unwrap();

        assert_eq!(Some("\"musti-test\""), row[0].as_str());
    }

    #[tokio::test]
    async fn should_map_nonexisting_database_error() {
        let mut url = Url::parse(&CONN_STR).unwrap();
        url.set_path("/this_does_not_exist");

        let res = Quaint::new(url.as_str()).await;

        assert!(res.is_err());

        match res {
            Ok(_) => unreachable!(),
            Err(e) => match e.kind() {
                ErrorKind::DatabaseDoesNotExist { db_name } => {
                    assert_eq!(Some("3D000"), e.original_code());
                    assert_eq!(
                        Some("database \"this_does_not_exist\" does not exist"),
                        e.original_message()
                    );
                    assert_eq!("this_does_not_exist", db_name.as_str())
                }
                kind => panic!("Expected `DatabaseDoesNotExist`, got {:?}", kind),
            },
        }
    }

    #[tokio::test]
    async fn should_map_tls_errors() {
        let mut url = Url::parse(&CONN_STR).expect("parsing url");
        url.set_query(Some("sslmode=require&sslaccept=strict"));

        let res = Quaint::new(url.as_str()).await;

        assert!(res.is_err());

        match res {
            Ok(_) => unreachable!(),
            Err(e) => match e.kind() {
                ErrorKind::TlsError { .. } => (),
                other => panic!("{:#?}", other),
            },
        }
    }

    #[tokio::test]
    async fn should_map_null_constraint_errors() {
        use crate::ast::*;

        let conn = Quaint::new(&CONN_STR).await.unwrap();

        conn.query_raw("DROP TABLE IF EXISTS should_map_null_constraint_errors_test", &[])
            .await
            .unwrap();

        conn.query_raw(
            "CREATE TABLE should_map_null_constraint_errors_test (id TEXT PRIMARY KEY, optional TEXT)",
            &[],
        )
        .await
        .unwrap();

        let err = conn
            .query(
                Insert::single_into("should_map_null_constraint_errors_test")
                    .value("id", ParameterizedValue::Null)
                    .into(),
            )
            .await
            .unwrap_err();

        match err.kind() {
            ErrorKind::NullConstraintViolation { constraint } => {
                assert_eq!(Some("23502"), err.original_code());
                assert_eq!(
                    Some("null value in column \"id\" violates not-null constraint"),
                    err.original_message()
                );
                assert_eq!(constraint, &DatabaseConstraint::Fields(vec!["id".into()]))
            }
            other => panic!("{:?}", other),
        }

        // Schema change null constraint violations now

        conn.query(
            Insert::single_into("should_map_null_constraint_errors_test")
                .value("id", "theid")
                .into(),
        )
        .await
        .unwrap();

        let err = conn
            .query_raw(
                "ALTER TABLE should_map_null_constraint_errors_test ALTER COLUMN optional SET NOT NULL",
                &[],
            )
            .await
            .unwrap_err();

        match err.kind() {
            ErrorKind::NullConstraintViolation { constraint } => {
                assert_eq!(Some("23502"), err.original_code());
                assert_eq!(Some("column \"optional\" contains null values"), err.original_message());
                assert_eq!(constraint, &DatabaseConstraint::Fields(vec!["optional".into()]))
            }
            other => panic!("{:?}", other),
        }
    }
}
