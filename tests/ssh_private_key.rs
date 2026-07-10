use devmanager::models::SSHConnection;

#[test]
fn private_key_serializes_camel_case_and_round_trips() {
    let connection = SSHConnection {
        id: "ssh-1".to_string(),
        label: "Prod".to_string(),
        host: "example.com".to_string(),
        port: 22,
        username: "deploy".to_string(),
        password: Some("pw".to_string()),
        private_key: Some(
            "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----\n"
                .to_string(),
        ),
    };

    let json = serde_json::to_string(&connection).expect("serialize");
    assert!(json.contains("\"privateKey\""));

    let back: SSHConnection = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, connection);
}

#[test]
fn legacy_connection_without_private_key_loads_as_none() {
    let json = r#"{"id":"ssh-1","label":"Prod","host":"example.com","port":22,"username":"deploy"}"#;

    let connection: SSHConnection = serde_json::from_str(json).expect("deserialize legacy");

    assert_eq!(connection.private_key, None);
    assert_eq!(connection.password, None);
}
