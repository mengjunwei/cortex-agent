//! 配置模块单元测试

use cortex_agent::config::{
    AppConfig, AuthConfig, DbConfig, LogConfig, RedisConfig, SecurityConfig, ServerConfig,
};

fn sample_db_config() -> DbConfig {
    DbConfig {
        db_type: "postgres".to_string(),
        host: "localhost".to_string(),
        port: 5432,
        password: "secret".to_string(),
        user: "admin".to_string(),
        db: "testdb".to_string(),
        connect_timeout: 10,
        statement_timeout: 30,
        pool_max_size: 10,
        pool_timeout: 5,
    }
}

fn sample_app_config() -> AppConfig {
    AppConfig {
        server: ServerConfig {
            port: "8090".to_string(),
        },
        db: sample_db_config(),
        redis: RedisConfig {
            host: "localhost".to_string(),
            port: 6379,
            password: "".to_string(),
        },
        log: LogConfig {
            debug: true,
            path: "/tmp/logs".to_string(),
            level: "DEBUG".to_string(),
        },
        kb: Default::default(),
        context: Default::default(),
        security: SecurityConfig::default(),
        auth: AuthConfig::default(),
        skill: Default::default(),
        workspace: Default::default(),
        shell: Default::default(),
        mcp: Default::default(),
        assistant: Default::default(),
        data_dir: "./data".to_string(),
    }
}

mod db_config {
    use super::*;

    #[test]
    fn test_url_postgres() {
        let cfg = sample_db_config();
        let url = cfg.url();
        assert!(url.contains("postgres://"));
        assert!(url.contains("localhost:5432"));
        assert!(url.contains("testdb"));
        assert!(url.contains("connect_timeout=10"));
        assert!(url.contains("statement_timeout=30000")); // ms
    }

    #[test]
    fn test_url_mysql() {
        let mut cfg = sample_db_config();
        cfg.db_type = "mysql".to_string();
        let url = cfg.url();
        assert!(url.contains("mysql://"));
        assert!(url.contains("connect_timeout=10"));
        assert!(url.contains("wait_timeout=30"));
    }

    #[test]
    fn test_url_encoding() {
        let mut cfg = sample_db_config();
        cfg.user = "user@domain".to_string();
        cfg.password = "p@ss=word".to_string();
        let url = cfg.url();
        assert!(url.contains("user%40domain"));
        assert!(url.contains("p%40ss%3Dword"));
    }
}

mod redis_config {
    use super::*;

    #[test]
    fn test_url_without_password() {
        let cfg = RedisConfig {
            host: "localhost".to_string(),
            port: 6379,
            password: "".to_string(),
        };
        assert_eq!(cfg.url(), "redis://localhost:6379/");
    }

    #[test]
    fn test_url_with_password() {
        let cfg = RedisConfig {
            host: "localhost".to_string(),
            port: 6379,
            password: "secret".to_string(),
        };
        assert_eq!(cfg.url(), "redis://:secret@localhost:6379/");
    }
}

mod app_config {
    use super::*;

    #[test]
    fn test_sample_config_values() {
        let cfg = sample_app_config();
        assert_eq!(cfg.server.port, "8090");
        assert!(cfg.log.debug);
    }
}

mod config_parsing {
    use super::*;

    #[test]
    fn test_load_valid_config() {
        let config_str = r#"
[server]
port = "8090"

[db]
db_type = "postgres"
host = "localhost"
port = 5432
password = "secret"
user = "admin"
db = "testdb"

[redis]
host = "localhost"
port = 6379
password = ""

[log]
debug = true
path = "/tmp/logs"
level = "DEBUG"
"#;
        let cfg: AppConfig = toml::from_str(config_str).unwrap();
        assert_eq!(cfg.server.port, "8090");
        assert_eq!(cfg.db.host, "localhost");
    }

    #[test]
    fn test_load_minimal_config() {
        let config_str = r#"
[server]

[db]
db_type = "postgres"
host = "localhost"
port = 5432
password = "pwd"
user = "u"
db = "d"

[redis]
host = "localhost"
port = 6379
password = ""

[log]
debug = false
path = "/tmp"
level = "INFO"
"#;
        let cfg: AppConfig = toml::from_str(config_str).unwrap();
        assert_eq!(cfg.server.port, "8090"); // default
        assert_eq!(cfg.log.level, "INFO");
    }
}
