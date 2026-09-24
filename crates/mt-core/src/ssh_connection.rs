use serde::{Deserialize, Serialize};

/// 一条已保存的 SSH 连接。持久化在 `config.json` 的 `sshConnections` 数组里。
///
/// 该类型被 mini-term 主程序与 SSH MCP sidecar 共用,因此放在 `mt-core`。
///
/// 所有连接默认都能被 SSH MCP 工具访问;具体哪个项目的 agent 能看到哪些连接,
/// 由 `config.json` 里项目的 `sshConnectionIds` 决定(见 `config_reader`),
/// 连接本身不再带可见性开关。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshConnection {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// 本版本不认识的字段(多半是更新的版本写进配置库的),原样带着往返 ——
    /// 用户在预览版 / 正式版之间来回装时,旧版本存一次配置不该把它们抹掉
    /// (口径见 mt-config `db.rs` 的「前向兼容」段)。
    ///
    /// - 只由反序列化填充:已知字段(含 `password`)先被各自的字段吃掉,落不进这里,
    ///   所以密码信封不会因此重复,也绕不过封存。**不许手工往里塞已知字段名** ——
    ///   flatten 序列化时会与真字段重复成两个同名键;
    /// - sidecar 读的 `config.json` 投影**不带**它(mt-config 写投影前清空),
    ///   sidecar 侧拿到的恒为空。
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_connection_deserializes() {
        let json = r#"{"id":"1","name":"prod","host":"10.0.0.5","port":22,"user":"root"}"#;
        let conn: SshConnection = serde_json::from_str(json).unwrap();
        assert_eq!(conn.id, "1");
        assert_eq!(conn.port, 22);
        assert!(conn.password.is_none());
    }

    #[test]
    fn connection_round_trips() {
        let conn = SshConnection {
            id: "abc".into(),
            name: "prod".into(),
            host: "example.com".into(),
            port: 2222,
            user: "deploy".into(),
            password: Some("secret".into()),
            identity_file: None,
            group: Some("内网".into()),
            extra: Default::default(),
        };
        let json = serde_json::to_string(&conn).unwrap();
        let parsed: SshConnection = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.port, 2222);
        assert_eq!(parsed.group.as_deref(), Some("内网"));
    }

    #[test]
    fn fields_use_camel_case() {
        let conn = SshConnection {
            id: "1".into(),
            name: "n".into(),
            host: "h".into(),
            port: 22,
            user: "u".into(),
            password: None,
            identity_file: Some("/k".into()),
            group: None,
            extra: Default::default(),
        };
        let json = serde_json::to_string(&conn).unwrap();
        assert!(json.contains("\"identityFile\":\"/k\""));
    }

    #[test]
    fn legacy_proxy_jump_field_is_ignored() {
        // 老配置文件里残留的 proxyJump 字段不破坏整体反序列化;它落进 `extra`
        // (入库前由 mt-config 的下线字段表剥掉)。
        let json = r#"{"id":"1","name":"n","host":"h","port":22,"user":"u","proxyJump":"user@bastion"}"#;
        let conn: SshConnection = serde_json::from_str(json).unwrap();
        assert_eq!(conn.id, "1");
        assert_eq!(
            conn.extra.get("proxyJump").and_then(|v| v.as_str()),
            Some("user@bastion")
        );
    }

    /// 未知字段原样往返;已知字段(尤其是密码)只进自己的字段、不落进 `extra`,
    /// 序列化出来也只有一个同名键 —— flatten 不能让密码信封重影。
    #[test]
    fn unknown_fields_round_trip_without_shadowing_known_ones() {
        let json = r#"{"id":"1","name":"n","host":"h","port":2222,"user":"u","password":"enc:v1:xyz","jumpHost":{"host":"b","port":22},"tags":["a"]}"#;
        let conn: SshConnection = serde_json::from_str(json).unwrap();
        assert_eq!(conn.port, 2222, "flatten 走缓冲反序列化,数字仍要认得出");
        assert_eq!(conn.password.as_deref(), Some("enc:v1:xyz"));
        assert!(!conn.extra.contains_key("password"));
        assert!(!conn.extra.contains_key("port"));
        assert_eq!(conn.extra.len(), 2);

        let out = serde_json::to_string(&conn).unwrap();
        assert_eq!(
            out.matches("\"password\"").count(),
            1,
            "密码键不能重复:{out}"
        );
        let back: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(back["jumpHost"]["host"], "b");
        assert_eq!(back["tags"][0], "a");
    }
}
