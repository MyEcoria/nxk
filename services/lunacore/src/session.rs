//! The cloud game session: what to ask `createGameSessionJob` for, the
//! job and session replies (field names as in the Android client's Gson
//! models), the regions to measure. Ported from the desktop client.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Status of a `createGameSessionJob` job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum JobStatus {
    ActiveRequested,
    ActiveQueued,
    ActiveCreatingSession,
    ActiveConnecting,
    ActiveTidyingUp,
    CompletedSucceeded,
    CompletedFailed,
    #[serde(other)]
    Unknown,
}

impl JobStatus {
    /// # True once the job will not change any more.
    pub fn is_final(self) -> bool {
        matches!(self, Self::CompletedSucceeded | Self::CompletedFailed)
    }
}

/// Reply of `createGameSessionJob`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateJobResponse {
    #[serde(default)]
    pub error_code: i64,
    pub game_session_job_id: Option<String>,
    #[serde(default)]
    pub start_polling_after_ms: u64,
}

/// Reply of `getGameSessionJob`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetJobResponse {
    #[serde(default)]
    pub error_code: i64,
    pub status: Option<JobStatus>,
    pub game_session: Option<GameSession>,
    #[serde(default)]
    pub retry_after_ms: u64,
    #[serde(default)]
    pub position_in_line: i64,
    #[serde(default)]
    pub expected_wait_time_seconds: i64,
}

/// Connection details of a running cloud session.
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameSession {
    pub session_id: Option<String>,
    pub public_ip: Option<String>,
    #[serde(default, deserialize_with = "port_from_number_or_string")]
    pub port: Option<u16>,
    pub region: Option<String>,
    pub connection_id: Option<String>,
    pub transport: Option<String>,
    pub protocol: Option<String>,
    pub routing: Option<String>,
    pub ip_version: Option<String>,
    pub auth_token: Option<String>,
    #[serde(alias = "gameId")]
    pub product_id: Option<String>,
    pub certificate_information: Option<Value>,
    pub accelerator_headers_information: Option<Value>,
    pub status: Option<String>,
}

/// The server sends the port as a string (`"33435"`); accept a number too.
fn port_from_number_or_string<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u16>, D::Error> {
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value
        .as_ref()
        .and_then(crate::as_u64)
        .and_then(|n| u16::try_from(n).ok()))
}

impl GameSession {
    /// # JSON for display with the secret `authToken` masked.
    pub fn redacted(&self) -> Value {
        let mut copy = self.clone();
        if copy.auth_token.is_some() {
            copy.auth_token = Some("<redacted>".into());
        }
        serde_json::to_value(copy).unwrap_or(Value::Null)
    }
}

impl std::fmt::Debug for GameSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.redacted())
    }
}

/// Launch parameters carried by a game page's `GAME_SESSION_ASYNC` action.
#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LaunchParams {
    pub product_id: String,
    pub build_id: Option<String>,
    pub product_stage: Option<String>,
    pub session_type: Option<String>,
    pub play_token: Option<String>,
    pub party_id: Option<String>,
    pub save_slot_id: Option<String>,
    pub title: Option<String>,
    #[serde(default)]
    pub revoke_profile_activity_request: bool,
    #[serde(default)]
    pub transfer_request: bool,
}

impl std::fmt::Debug for LaunchParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchParams")
            .field("product_id", &self.product_id)
            .field("title", &self.title)
            .field("build_id", &self.build_id)
            .field(
                "play_token",
                &self.play_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// # Builds a `createGameSessionJob` request, like the Android client does.
///
/// ## Arguments
/// * `launch` - parameters from the game page
/// * `region_latencies` / `region_preferences` - see [`crate::api::regions`]
/// * `(width, height)` - display size to request
pub fn create_job_request(
    launch: &LaunchParams,
    region_latencies: Value,
    region_preferences: Value,
    (width, height): (u32, u32),
) -> Value {
    let mut request = json!({
        "productId": launch.product_id,
        "sessionType": launch.session_type.as_deref().unwrap_or("GAME"),
        "revokeProfileActivityRequest": launch.revoke_profile_activity_request,
        "transferRequest": launch.transfer_request,
        "clientFeatures": {
            "codecs": [{"codec": "AVC", "profile": 8, "level": 65536}],
            "dataUsage": "STANDARD",
            "display": {"width": width, "height": height},
            "requestHdr": false,
            "routing": ["AGA", "DIRECT"],
            "timezone": "UTC",
        },
        "regionLatencies": region_latencies,
        "regionPreferences": region_preferences,
    });
    let optional = [
        ("buildId", &launch.build_id),
        ("productStage", &launch.product_stage),
        ("playToken", &launch.play_token),
        ("partyId", &launch.party_id),
        ("saveSlotId", &launch.save_slot_id),
    ];
    for (key, value) in optional {
        if let Some(value) = value {
            request[key] = json!(value);
        }
    }
    request
}

/// Luna's regions, measured for `regionLatencies`.
/// Regions known to the Android client (`AppConfigManagerConstants`).
pub const REGIONS: [&str; 7] = [
    "us-east-1",
    "us-east-2",
    "us-west-1",
    "us-west-2",
    "eu-central-1",
    "eu-north-1",
    "eu-west-3",
];

/// # URL of a region's ping endpoint.
///
/// ## Examples
/// ```
/// assert_eq!(lunacore::session::ping_url("eu-west-3"),
///            "https://eu-west-3.prod.routing.sips.tempo.digital.a2z.com/pingRegion");
/// ```
pub fn ping_url(region: &str) -> String {
    format!("https://{region}.prod.routing.sips.tempo.digital.a2z.com/pingRegion")
}

/// # Converts measurements into the `regionLatencies` / `regionPreferences` JSON.
///
/// ## Examples
/// ```
/// let (lat, pref) = lunacore::session::to_json(&[("eu-west-3".into(), 12.5)]);
/// assert_eq!(lat[0]["region"], "eu-west-3");
/// assert_eq!(pref[0]["region"], "eu-west-3");
/// ```
pub fn to_json(measured: &[(String, f64)]) -> (Value, Value) {
    let latencies = measured
        .iter()
        .map(|(region, ms)| json!({"region": region, "endpoint": format!("dynamodb.{region}.amazonaws.com"), "latencyMs": ms}))
        .collect::<Vec<_>>();
    let preferences = measured
        .iter()
        .map(|(region, _)| json!({"region": region}))
        .collect::<Vec<_>>();
    (Value::Array(latencies), Value::Array(preferences))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_succeeded_job_with_a_session() {
        let reply: GetJobResponse = serde_json::from_value(json!({
            "status": "COMPLETED_SUCCEEDED", "errorCode": 0,
            "gameSession": {"sessionId":"s","publicIp":"1.2.3.4","port":8443,"transport":"UDP","authToken":"SECRET","gameId":"p"}
        }))
        .unwrap();
        assert_eq!(reply.status, Some(JobStatus::CompletedSucceeded));
        let gs = reply.game_session.unwrap();
        assert_eq!(
            (gs.public_ip.as_deref(), gs.port, gs.product_id.as_deref()),
            (Some("1.2.3.4"), Some(8443), Some("p"))
        );
    }

    #[test]
    fn parses_the_session_shape_observed_live() {
        let reply: GetJobResponse = serde_json::from_value(json!({
            "status": "COMPLETED_SUCCEEDED",
            "gameSession": {"authToken": "X", "connectionId": "42", "gameId": "g", "ipVersion": "IPV4", "port": "33435",
                "protocol": "DCV", "publicIp": "1.2.3.4", "region": "eu-central-1", "routing": "DIRECT",
                "sessionId": "s", "status": "AVAILABLE", "transport": "UDP",
                "certificateInformation": {"commonName": "c", "sha256": "h", "type": "SIPS"}}
        }))
        .unwrap();
        let gs = reply.game_session.unwrap();
        assert_eq!(
            (gs.port, gs.protocol.as_deref(), gs.transport.as_deref()),
            (Some(33435), Some("DCV"), Some("UDP"))
        );
        assert_eq!(gs.region.as_deref(), Some("eu-central-1"));
    }

    #[test]
    fn port_out_of_range_becomes_none() {
        let gs: GameSession = serde_json::from_value(json!({"port": "70000"})).unwrap();
        assert_eq!(gs.port, None);
    }

    #[test]
    fn unknown_status_does_not_break_parsing() {
        let reply: GetJobResponse =
            serde_json::from_value(json!({"status": "SOMETHING_NEW"})).unwrap();
        assert_eq!(reply.status, Some(JobStatus::Unknown));
    }

    #[test]
    fn auth_token_never_appears_in_debug_output() {
        let gs: GameSession =
            serde_json::from_value(json!({"authToken": "SECRET", "publicIp": "1.2.3.4"})).unwrap();
        let shown = format!("{gs:?} {}", gs.redacted());
        assert!(!shown.contains("SECRET"));
        assert!(shown.contains("<redacted>"));
    }

    #[test]
    fn only_completed_statuses_are_final() {
        assert!(JobStatus::CompletedFailed.is_final());
        assert!(!JobStatus::ActiveQueued.is_final());
    }

    fn launch() -> LaunchParams {
        serde_json::from_value(json!({
            "productId": "PID", "buildId": "B", "productStage": "Release", "playToken": "SECRET-PLAY",
            "sessionType": "GAME", "title": "Game", "transferRequest": false
        }))
        .unwrap()
    }

    #[test]
    fn request_carries_launch_params_and_display() {
        let req = create_job_request(&launch(), json!([]), json!([]), (1920, 1080));
        assert_eq!(req["productId"], "PID");
        assert_eq!(req["buildId"], "B");
        assert_eq!(req["playToken"], "SECRET-PLAY");
        assert_eq!(req["clientFeatures"]["display"]["width"], 1920);
        assert_eq!(req["clientFeatures"]["routing"][1], "DIRECT");
    }

    #[test]
    fn absent_optional_fields_are_not_sent() {
        let minimal: LaunchParams = serde_json::from_value(json!({"productId": "P"})).unwrap();
        let req = create_job_request(&minimal, json!([]), json!([]), (1, 1));
        assert!(req.get("playToken").is_none() && req.get("buildId").is_none());
        assert_eq!(req["sessionType"], "GAME");
    }

    #[test]
    fn play_token_is_redacted_in_debug_output() {
        assert!(!format!("{:?}", launch()).contains("SECRET-PLAY"));
    }
}
