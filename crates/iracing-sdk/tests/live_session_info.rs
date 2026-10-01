#![cfg(windows)]

use iracing_sdk::{provider::Provider, providers::live::LiveProvider, schema::SessionInfo};

#[tokio::test]
#[ignore = "requires iRacing running in an active session"]
async fn parses_live_iracing_session_info() {
    let mut provider = LiveProvider::new().expect("connect to running iRacing");
    let yaml = provider
        .session_yaml(0)
        .await
        .expect("acquire live session YAML")
        .expect("active session should expose session YAML");
    let session = SessionInfo::parse(&yaml).expect("live session should deserialize");
    assert!(!session.weekend_info.track_name.is_empty());
    assert!(!session.weekend_info.track_display_name.is_empty());
    assert!(!session.session_info.sessions.is_empty());
}
