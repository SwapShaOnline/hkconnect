//! End-to-end tests against a fake server that replays real (scrubbed)
//! responses recorded from a 2024 Hyundai Creta.

#![expect(
    clippy::needless_pass_by_value,
    clippy::items_after_statements,
    reason = "test helpers favour readability"
)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use hkconnect::{
    Brand, Celsius, Client, ClimateOptions, CommandId, CommandStatus, Config, Credentials,
    Destination, Error, Powertrain,
};
use pretty_assertions::assert_eq;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use wiremock::matchers::{body_json, body_string_contains, header, header_exists, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const VEHICLE: &str = "00000000-0000-4000-8000-000000000001";
const DEVICE: &str = "00000000-0000-4000-8000-0000000000de";

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

fn envelope(res_msg: Value, msg_id: &str) -> Value {
    json!({ "retCode": "S", "resCode": "0000", "resMsg": res_msg, "msgId": msg_id })
}

fn credentials(pin: Option<&str>) -> Credentials {
    let credentials = Credentials::new("owner@example.com", "hunter2");
    match pin {
        Some(pin) => credentials.with_pin(pin),
        None => credentials,
    }
}

fn client(server: &MockServer, pin: Option<&str>) -> Client {
    let config = Config::india(Brand::Hyundai).with_base_url(server.uri().parse().unwrap());
    Client::new(config, credentials(pin)).unwrap()
}

/// Mount the four login steps; `logins` is how many full logins to expect.
async fn mount_login(server: &MockServer, logins: u64) {
    mount_login_with_token(
        server,
        logins,
        ok(json!({
            "access_token": "access-1", "token_type": "Bearer",
            "refresh_token": "refresh-1", "expires_in": 3600
        })),
    )
    .await;
}

/// [`mount_login`] with a custom answer from the token endpoint.
async fn mount_login_with_token(server: &MockServer, logins: u64, token: impl Respond + 'static) {
    Mock::given(method("POST"))
        .and(path("/api/v1/spa/notifications/register"))
        .and(header_exists("Stamp"))
        .and(header(
            "ccsp-service-id",
            "e5b3f6d0-7f83-43c9-aff3-a254db7af368",
        ))
        .respond_with(ok(fixture("register")))
        .up_to_n_times(logins)
        .expect(logins)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/user/oauth2/authorize"))
        .respond_with(ResponseTemplate::new(200).insert_header("set-cookie", "account=abc; Path=/"))
        .expect(logins)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/user/signin"))
        .and(header("cookie", "account=abc"))
        .and(body_json(
            json!({ "email": "owner@example.com", "password": "hunter2" }),
        ))
        .respond_with(ok(fixture("signin")))
        .expect(logins)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/user/oauth2/token"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code=REDACTED"))
        .and(header_exists("Stamp"))
        .respond_with(token)
        .expect(logins)
        .mount(server)
        .await;
}

async fn mount_vehicles(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/api/v1/spa/vehicles"))
        .and(header("Authorization", "Bearer access-1"))
        .and(header("ccsp-device-id", DEVICE))
        .respond_with(ok(fixture("vehicles")))
        .mount(server)
        .await;
}

#[tokio::test]
async fn reads_everything_with_a_single_login() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    mount_vehicles(&server).await;
    for (suffix, name) in [
        ("status/latest", "status_latest"),
        ("location/park", "location"),
        ("profile", "profile"),
        ("setting/alert/maintenance", "maintenance"),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/api/v1/spa/vehicles/{VEHICLE}/{suffix}")))
            .respond_with(ok(fixture(name)))
            .mount(&server)
            .await;
    }

    let client = client(&server, None);
    let car = client.vehicles().await.unwrap().remove(0);
    assert_eq!(car.model, "Creta");
    assert_eq!(car.powertrain, Powertrain::Petrol);
    assert!(!car.ccs2);

    let status = client.status(&car).await.unwrap();
    assert_eq!(status.locked, Some(false));
    assert_eq!(status.engine_on, Some(false));
    assert_eq!(status.fuel_percent, Some(63.0));
    assert_eq!(status.range.unwrap().value, Some(455.0));
    assert!(!status.anything_open());
    // The car reports "00H" (14 °C) while climate is off; that must not leak out.
    assert_eq!(status.climate_temperature, None);
    assert_eq!(
        status.updated_at.unwrap().to_string(),
        "2026-11-23T10:41:34Z"
    );

    let location = client.location(&car).await.unwrap().unwrap();
    assert!((location.latitude - 12.971_598_7).abs() < 1e-9);

    let caps = client.capabilities(&car).await.unwrap();
    assert!(caps.remote_start && caps.horn_and_lights && caps.seat_ventilation);
    assert!(!caps.steering_wheel_heat);
    assert_eq!(caps.transmission.as_deref(), Some("AT"));

    let maintenance = client.maintenance(&car).await.unwrap();
    assert_eq!(maintenance.odometer_km, Some(42412.0));
    let oil = maintenance
        .items
        .iter()
        .find(|i| i.name == "Engine oil")
        .unwrap();
    assert_eq!(
        (oil.interval.km, oil.interval.months),
        (Some(10000), Some(12))
    );
    // `mount_login(.., 1)` verifies on drop that we logged in exactly once.
}

#[tokio::test]
async fn saved_session_skips_login() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    mount_vehicles(&server).await;

    let session = client(&server, None).login().await.unwrap();
    let restored = client(&server, None).with_session(session);
    assert_eq!(restored.vehicles().await.unwrap().len(), 1);
}

#[tokio::test]
async fn logs_in_again_when_the_token_is_rejected() {
    let server = MockServer::start().await;
    mount_login(&server, 2).await;

    // First call says the token expired; the retry succeeds.
    struct ExpireOnce(std::sync::atomic::AtomicBool);
    impl Respond for ExpireOnce {
        fn respond(&self, _: &Request) -> ResponseTemplate {
            if self.0.swap(true, std::sync::atomic::Ordering::SeqCst) {
                ok(fixture("vehicles"))
            } else {
                ResponseTemplate::new(401)
                    .set_body_json(json!({ "error": "Key not authorized: Token is expired" }))
            }
        }
    }
    Mock::given(method("GET"))
        .and(path("/api/v1/spa/vehicles"))
        .respond_with(ExpireOnce(false.into()))
        .expect(2)
        .mount(&server)
        .await;

    let client = client(&server, None);
    client.login().await.unwrap();
    assert_eq!(client.vehicles().await.unwrap().len(), 1);
}

#[tokio::test]
async fn server_errors_become_typed_errors() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/spa/vehicles"))
        .respond_with(ok(
            json!({ "retCode": "F", "resCode": "5091", "resMsg": "Exceeds number of requests" }),
        ))
        .mount(&server)
        .await;

    let err = client(&server, None).vehicles().await.unwrap_err();
    assert!(matches!(err, Error::RateLimited), "{err:?}");
}

#[tokio::test]
async fn lock_then_wait_for_the_car() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    mount_vehicles(&server).await;
    Mock::given(method("POST"))
        .and(path(format!("/api/v1/spa/vehicles/{VEHICLE}/control/door")))
        .and(body_json(json!({ "action": "close", "deviceId": DEVICE })))
        .respond_with(ok(envelope(Value::Null, "cmd-1")))
        .expect(1)
        .mount(&server)
        .await;

    // Pending on the first poll, done on the second.
    struct Progress(std::sync::atomic::AtomicU8);
    impl Respond for Progress {
        fn respond(&self, _: &Request) -> ResponseTemplate {
            let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let result = if n == 0 {
                Value::Null
            } else {
                json!("success")
            };
            ok(envelope(
                json!([{ "recordId": "cmd-1", "result": result }]),
                "x",
            ))
        }
    }
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/spa/notifications/{VEHICLE}/records")))
        .respond_with(Progress(0.into()))
        .mount(&server)
        .await;

    let client = client(&server, None);
    let car = client.vehicles().await.unwrap().remove(0);
    let id = client.lock(&car).await.unwrap();
    assert_eq!(id.as_str(), "cmd-1");
    let status = client
        .wait_for(&car, &id, Duration::from_secs(5), Duration::from_millis(10))
        .await
        .unwrap();
    assert_eq!(status, CommandStatus::Succeeded);
}

#[tokio::test]
async fn remote_start_sends_the_official_payload() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    mount_vehicles(&server).await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/v1/spa/vehicles/{VEHICLE}/control/engine"
        )))
        .and(body_json(json!({
            "action": "start",
            "hvacType": 1,
            "options": { "defrost": true, "heating1": 0, "igniOnDuration": 5 },
            "tempCode": "10H",
            "unit": "C",
        })))
        .respond_with(ok(envelope(Value::Null, "cmd-2")))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server, None);
    let car = client.vehicles().await.unwrap().remove(0);
    let options = ClimateOptions::new(Celsius::new(22.0).unwrap())
        .with_duration_minutes(5)
        .unwrap()
        .with_defrost(true);
    assert_eq!(
        client.start_climate(&car, &options).await.unwrap().as_str(),
        "cmd-2"
    );

    // An invalid duration can't even be built, so it can't be sent.
    let too_long = options.with_duration_minutes(30).unwrap_err();
    assert!(matches!(too_long, Error::InvalidDuration(30)));
    assert!(!too_long.may_have_been_received());
}

#[tokio::test]
async fn pin_commands_use_a_control_token() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    mount_vehicles(&server).await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/user/pin"))
        .and(body_json(json!({ "deviceId": DEVICE, "pin": "1234" })))
        .respond_with(ok(json!({ "controlToken": "ctl-1", "expiresTime": 600 })))
        .expect(1) // cached for the second command
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/v2/spa/vehicles/{VEHICLE}/control/engine"
        )))
        .and(header("Authorization", "Bearer ctl-1"))
        .and(header("AuthorizationCCSP", "Bearer ctl-1"))
        .and(body_json(json!({ "action": "stop" })))
        .respond_with(ok(envelope(Value::Null, "cmd-3")))
        .expect(2)
        .mount(&server)
        .await;

    let client = client(&server, Some("1234"));
    let car = client.vehicles().await.unwrap().remove(0);
    client.stop_climate(&car).await.unwrap();
    client.stop_climate(&car).await.unwrap();
}

#[tokio::test]
async fn pin_commands_fail_cleanly_without_a_pin_or_with_a_wrong_one() {
    let server = MockServer::start().await;
    mount_login(&server, 2).await;
    mount_vehicles(&server).await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/user/pin"))
        .respond_with(ok(json!({ "errCode": "4011", "errMsg": "Invalid PIN" })))
        .mount(&server)
        .await;

    let no_pin = client(&server, None);
    let car = no_pin.vehicles().await.unwrap().remove(0);
    assert!(matches!(
        no_pin.stop_climate(&car).await,
        Err(Error::PinRequired)
    ));

    let wrong_pin = client(&server, Some("0000"));
    assert!(matches!(
        wrong_pin.stop_climate(&car).await,
        Err(Error::InvalidPin)
    ));
}

#[tokio::test]
async fn ccs2_cars_get_a_clear_unsupported_error() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    let mut vehicles = fixture("vehicles");
    vehicles["resMsg"]["vehicles"][0]["ccuCCS2ProtocolSupport"] = json!(1);
    Mock::given(method("GET"))
        .and(path("/api/v1/spa/vehicles"))
        .respond_with(ok(vehicles))
        .mount(&server)
        .await;

    let client = client(&server, None);
    let car = client.vehicles().await.unwrap().remove(0);
    assert!(car.ccs2);
    assert!(matches!(
        client.status(&car).await,
        Err(Error::UnsupportedVehicle(_))
    ));
}

#[tokio::test]
async fn re_registers_the_device_when_the_server_forgets_it() {
    let server = MockServer::start().await;
    // One login registers once; the 4002 recovery registers a second time.
    mount_login(&server, 1).await;
    Mock::given(method("POST"))
        .and(path("/api/v1/spa/notifications/register"))
        .respond_with(ok(envelope(json!({ "deviceId": "new-device" }), "r")))
        .expect(1)
        .with_priority(10) // only after the login's registration is used up
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/spa/vehicles"))
        .and(header("ccsp-device-id", DEVICE))
        .respond_with(ok(json!({ "retCode": "F", "resCode": "4002", "resMsg": "Invalid request body - invalid deviceId" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/spa/vehicles"))
        .and(header("ccsp-device-id", "new-device"))
        .respond_with(ok(fixture("vehicles")))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server, None);
    client.login().await.unwrap();
    assert_eq!(client.vehicles().await.unwrap().len(), 1);
}

#[tokio::test]
async fn trip_history_month_day_and_detail() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    mount_vehicles(&server).await;
    let trips = format!("/api/v1/spa/vehicles/{VEHICLE}/tripinfo");
    Mock::given(method("POST"))
        .and(path(trips.clone()))
        .and(body_json(
            json!({ "tripPeriodType": 0, "setTripMonth": "202611" }),
        ))
        .respond_with(ok(fixture("trip_month")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(trips))
        .and(body_json(
            json!({ "tripPeriodType": 1, "setTripDay": "20261117" }),
        ))
        .respond_with(ok(fixture("trip_day")))
        .mount(&server)
        .await;
    // The detail request must echo the raw identifiers from the day listing.
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/v1/spa/vehicles/{VEHICLE}/tripinfo/detail"
        )))
        .and(body_json(json!({
            "tripPeriodType": 1,
            "setTripDay": "20261117",
            "setTripStartTime": "20261117054442",
            "setServiceTID": 20_261_117_054_442_u64,
            "tripStartTime": "20261117054442",
            "tripEndTime": "20261117060156",
        })))
        .respond_with(ok(fixture("trip_detail")))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server, None);
    let car = client.vehicles().await.unwrap().remove(0);

    let month = client.trips_in_month(&car, 2026, 11).await.unwrap();
    assert_eq!(month.days.len(), 13);
    assert_eq!(month.days[0].date.to_string(), "2026-11-09");
    assert_eq!(month.days[0].trips, 4);
    assert_eq!(month.totals.distance_km, Some(486.0));

    let date = jiff::civil::date(2026, 11, 17);
    let day = client.trips_on_day(&car, date).await.unwrap();
    assert_eq!(day.trips.len(), 2);
    assert_eq!(day.totals.drive_minutes, Some(30));
    let first = &day.trips[0];
    // Sorted chronologically: 05:44:42 IST (00:14:42 UTC) comes first.
    assert_eq!(
        first.started_at.unwrap().to_string(),
        "2026-11-17T00:14:42Z"
    );
    assert!(day.trips[0].started_at < day.trips[1].started_at);
    assert!(first.start.is_some());

    let detail = client.trip_detail(&car, first).await.unwrap();
    assert_eq!(detail.totals.max_speed_kmh, Some(91.0));
    assert_eq!(detail.hard_braking, Some(0));
    assert_eq!(detail.route.len(), 4);
    assert_eq!(
        detail.route[0].at.unwrap().to_string(),
        "2026-11-17T02:33:45Z"
    );
}

#[tokio::test]
async fn valet_and_destination_are_pin_commands() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    mount_vehicles(&server).await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/user/pin"))
        .respond_with(ok(json!({ "controlToken": "ctl-1", "expiresTime": 600 })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/v2/spa/vehicles/{VEHICLE}/control/valet"
        )))
        .and(header("Authorization", "Bearer ctl-1"))
        .and(body_json(json!({ "action": "activate" })))
        .respond_with(ok(envelope(Value::Null, "valet-1")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/v2/spa/vehicles/{VEHICLE}/location/routes"
        )))
        .and(header("Authorization", "Bearer ctl-1"))
        .and(body_json(json!({
            "deviceID": DEVICE,
            "poiInfoList": [{
                "phone": "", "waypointID": 1, "lang": 1, "src": "HERE",
                "coord": { "lat": 12.9716, "lon": 77.5946, "alt": 0, "type": 0 },
                "addr": "MG Road", "zip": "", "placeid": "", "name": "Office",
            }],
        })))
        .respond_with(ok(envelope(Value::Null, "nav-1")))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server, Some("1234"));
    let car = client.vehicles().await.unwrap().remove(0);
    assert_eq!(
        client.valet_mode(&car, true).await.unwrap().as_str(),
        "valet-1"
    );
    let office = Destination::new("Office", 12.9716, 77.5946)
        .unwrap()
        .with_address("MG Road");
    assert_eq!(
        client
            .send_destination(&car, &office)
            .await
            .unwrap()
            .as_str(),
        "nav-1"
    );
}

#[tokio::test]
async fn numbers_sent_as_strings_still_parse() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    mount_vehicles(&server).await;
    let mut maintenance = fixture("maintenance");
    maintenance["resMsg"]["mainList"][0]["items"]["distValue"] = json!("30000");
    maintenance["resMsg"]["odometer"] = json!("42412");
    Mock::given(method("GET"))
        .and(path(format!(
            "/api/v1/spa/vehicles/{VEHICLE}/setting/alert/maintenance"
        )))
        .respond_with(ok(maintenance))
        .mount(&server)
        .await;

    let client = client(&server, None);
    let car = client.vehicles().await.unwrap().remove(0);
    let m = client.maintenance(&car).await.unwrap();
    assert_eq!(m.odometer_km, Some(42412.0));
    assert_eq!(m.items[0].interval.km, Some(30000));
}

#[tokio::test]
async fn ccs2_cars_refuse_every_command_before_sending() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    let mut vehicles = fixture("vehicles");
    vehicles["resMsg"]["vehicles"][0]["ccuCCS2ProtocolSupport"] = json!(1);
    Mock::given(method("GET"))
        .and(path("/api/v1/spa/vehicles"))
        .respond_with(ok(vehicles))
        .mount(&server)
        .await;
    // No command endpoint is mounted: any request sent would fail differently.

    let client = client(&server, Some("1234"));
    let car = client.vehicles().await.unwrap().remove(0);
    let dest = Destination::new("x", 0.0, 0.0).unwrap();
    let results = [
        client.lock(&car).await,
        client.unlock(&car).await,
        client.start_climate(&car, &ClimateOptions::default()).await,
        client.stop_climate(&car).await,
        client.hazard_lights(&car, false).await,
        client.valet_mode(&car, true).await,
        client.send_destination(&car, &dest).await,
    ];
    for r in results {
        let err = r.unwrap_err();
        assert!(matches!(err, Error::UnsupportedVehicle(_)), "{err:?}");
        assert!(!err.may_have_been_received());
    }
}

#[tokio::test]
async fn invalid_month_is_rejected_locally() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    mount_vehicles(&server).await;
    let client = client(&server, None);
    let car = client.vehicles().await.unwrap().remove(0);
    assert!(matches!(
        client.trips_in_month(&car, 2026, 13).await,
        Err(Error::InvalidMonth(13))
    ));
}

#[tokio::test]
async fn wait_for_reports_unknown_if_the_command_never_appears() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    mount_vehicles(&server).await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/spa/notifications/{VEHICLE}/records")))
        .respond_with(ok(fixture("records_empty")))
        .mount(&server)
        .await;

    let client = client(&server, None);
    let car = client.vehicles().await.unwrap().remove(0);
    let id = CommandId::new("missing");
    let status = client
        .wait_for(
            &car,
            &id,
            Duration::from_millis(50),
            Duration::from_millis(10),
        )
        .await
        .unwrap();
    assert_eq!(status, CommandStatus::Unknown);
    assert!(!status.is_final() && !CommandStatus::Pending.is_final());
    assert!(CommandStatus::Failed.is_final());
}

/// Answers with each template in turn, repeating the last one.
struct Sequence(Vec<ResponseTemplate>, AtomicU32);

impl Sequence {
    fn new(templates: Vec<ResponseTemplate>) -> Self {
        Self(templates, AtomicU32::new(0))
    }
}

impl Respond for Sequence {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let n = self.1.fetch_add(1, Ordering::SeqCst) as usize;
        self.0[n.min(self.0.len() - 1)].clone()
    }
}

/// A vehicle restored from the library's own serialized form, without
/// listing the account's vehicles first.
fn car() -> hkconnect::Vehicle {
    serde_json::from_value(json!({
        "id": VEHICLE,
        "vin": "VIN",
        "name": "Car",
        "model": "Creta",
        "year": null,
        "powertrain": "petrol",
        "ccs2": false,
    }))
    .unwrap()
}

fn door_path() -> String {
    format!("/api/v1/spa/vehicles/{VEHICLE}/control/door")
}

#[tokio::test]
async fn gateway_timeout_on_unlock_is_never_resent() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await; // verified on drop: no re-login
    Mock::given(method("POST"))
        .and(path(door_path()))
        .respond_with(
            ResponseTemplate::new(504)
                .set_body_json(json!({ "error": "Upstream request timed out" })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server, None);
    let err = client.unlock(&car()).await.unwrap_err();
    assert!(matches!(err, Error::ServiceUnavailable), "{err:?}");
    assert!(err.may_have_been_received());
}

#[tokio::test]
async fn http_429_is_rate_limited_without_relogin() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    let limited = ResponseTemplate::new(429).set_body_json(json!({ "error": "Too many requests" }));
    Mock::given(method("POST"))
        .and(path(door_path()))
        .respond_with(limited.clone())
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/spa/vehicles"))
        .respond_with(limited)
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server, None);
    assert!(matches!(client.lock(&car()).await, Err(Error::RateLimited)));
    assert!(matches!(client.vehicles().await, Err(Error::RateLimited)));
}

#[tokio::test]
async fn command_rejected_with_7501_logs_in_and_is_resent_once() {
    let server = MockServer::start().await;
    mount_login(&server, 2).await;
    let rejected = ok(json!({ "retCode": "F", "resCode": "7501", "resMsg": "token expired" }));
    Mock::given(method("POST"))
        .and(path(door_path()))
        .respond_with(Sequence::new(vec![
            rejected,
            ok(envelope(Value::Null, "cmd-9")),
        ]))
        .expect(2)
        .mount(&server)
        .await;

    let client = client(&server, None);
    client.login().await.unwrap();
    assert_eq!(client.unlock(&car()).await.unwrap().as_str(), "cmd-9");
}

#[tokio::test]
async fn command_recovery_happens_at_most_once() {
    let server = MockServer::start().await;
    mount_login(&server, 2).await;
    Mock::given(method("POST"))
        .and(path(door_path()))
        .respond_with(ok(
            json!({ "retCode": "F", "resCode": "7501", "resMsg": "token expired" }),
        ))
        .expect(2)
        .mount(&server)
        .await;

    let client = client(&server, None);
    client.login().await.unwrap();
    assert!(matches!(
        client.unlock(&car()).await,
        Err(Error::Authentication(_))
    ));
}

#[tokio::test]
async fn null_error_and_huge_expiry_in_token_response_are_harmless() {
    let server = MockServer::start().await;
    mount_login_with_token(
        &server,
        1,
        ok(json!({
            "access_token": "access-1", "token_type": "Bearer",
            "refresh_token": "refresh-1", "expires_in": i64::MAX, "error": null
        })),
    )
    .await;

    let session = client(&server, None).login().await.unwrap();
    let month = jiff::SignedDuration::from_hours(31 * 24);
    assert!(session.expires_at() < jiff::Timestamp::now() + month);
    assert!(session.expires_at() > jiff::Timestamp::now());
}

#[tokio::test]
async fn huge_control_token_expiry_does_not_panic() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/user/pin"))
        .respond_with(ok(
            json!({ "controlToken": "ctl-1", "expiresTime": i64::MAX }),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/v2/spa/vehicles/{VEHICLE}/control/engine"
        )))
        .respond_with(ok(envelope(Value::Null, "cmd-3")))
        .mount(&server)
        .await;

    let client = client(&server, Some("1234"));
    client.stop_climate(&car()).await.unwrap();
}

#[tokio::test]
async fn pin_request_rejected_with_401_logs_in_again() {
    let server = MockServer::start().await;
    mount_login(&server, 2).await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/user/pin"))
        .respond_with(Sequence::new(vec![
            ResponseTemplate::new(401).set_body_string("Unauthorized"),
            ok(json!({ "controlToken": "ctl-1", "expiresTime": 600 })),
        ]))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/v2/spa/vehicles/{VEHICLE}/control/engine"
        )))
        .and(header("Authorization", "Bearer ctl-1"))
        .respond_with(ok(envelope(Value::Null, "cmd-3")))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server, Some("1234"));
    client.login().await.unwrap();
    assert_eq!(client.stop_climate(&car()).await.unwrap().as_str(), "cmd-3");
}

#[tokio::test]
async fn a_stale_rejection_does_not_wipe_a_fresh_session() {
    let server = MockServer::start().await;
    // Initial login, plus one recovery. A third login would mean the slow,
    // stale 401 cleared the session another task had just renewed.
    let token = |access: &str| {
        ok(json!({
            "access_token": access, "token_type": "Bearer",
            "refresh_token": "refresh-1", "expires_in": 3600
        }))
    };
    mount_login_with_token(
        &server,
        2,
        Sequence::new(vec![token("access-1"), token("access-2")]),
    )
    .await;
    let unauthorized = || ResponseTemplate::new(401).set_body_json(json!({ "error": "expired" }));
    Mock::given(method("GET"))
        .and(path("/api/v1/spa/vehicles"))
        .and(header("Authorization", "Bearer access-1"))
        .respond_with(unauthorized().set_delay(Duration::from_millis(500)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/spa/vehicles"))
        .and(header("Authorization", "Bearer access-2"))
        .respond_with(ok(fixture("vehicles")))
        .mount(&server)
        .await;
    let profile = format!("/api/v1/spa/vehicles/{VEHICLE}/profile");
    Mock::given(method("GET"))
        .and(path(profile.clone()))
        .and(header("Authorization", "Bearer access-1"))
        .respond_with(unauthorized())
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(profile))
        .and(header("Authorization", "Bearer access-2"))
        .respond_with(ok(fixture("profile")))
        .mount(&server)
        .await;

    let client = client(&server, None);
    client.login().await.unwrap();
    let car = car();
    let (slow, fast) = tokio::join!(client.vehicles(), client.capabilities(&car));
    slow.unwrap();
    fast.unwrap();
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    Mock::given(method("POST"))
        .and(path(door_path()))
        .respond_with(ResponseTemplate::new(307).insert_header("location", "/api/v1/elsewhere"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/elsewhere"))
        .respond_with(ok(envelope(Value::Null, "nope")))
        .expect(0)
        .mount(&server)
        .await;

    let err = client(&server, None).unlock(&car()).await.unwrap_err();
    assert!(matches!(err, Error::UnexpectedResponse(_)), "{err:?}");
}

fn expired_session() -> hkconnect::Session {
    serde_json::from_value(json!({
        "access_token": "Bearer old",
        "refresh_token": "refresh-0",
        "expires_at": "2000-01-01T00:00:00Z",
        "device_id": DEVICE,
    }))
    .unwrap()
}

#[tokio::test]
async fn refresh_outage_is_returned_not_turned_into_a_login() {
    let server = MockServer::start().await;
    // No login may start: its first step is registering the device.
    Mock::given(method("POST"))
        .and(path("/api/v1/spa/notifications/register"))
        .respond_with(ok(fixture("register")))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/user/oauth2/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(503).set_body_string("down"))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server, None).with_session(expired_session());
    let err = client.vehicles().await.unwrap_err();
    assert!(matches!(err, Error::ServiceUnavailable), "{err:?}");
}

#[tokio::test]
async fn refused_refresh_token_falls_back_to_login() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    mount_vehicles(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/v1/user/oauth2/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({ "error": "invalid_grant" })))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server, None).with_session(expired_session());
    assert_eq!(client.vehicles().await.unwrap().len(), 1);
}

#[tokio::test]
async fn debug_output_hides_account_and_app_secrets() {
    let server = MockServer::start().await;
    let out = format!("{:?}", client(&server, Some("1234")));
    for secret in ["owner@example.com", "hunter2", "1234", "Basic "] {
        assert!(!out.contains(secret), "{secret} leaked: {out}");
    }
}

// ----- lenient parsing: odd shapes from other models must not break calls -----

/// Mount a `GET /api/v1/spa/vehicles/{VEHICLE}/{suffix}` answer.
async fn mount_vehicle_get(server: &MockServer, suffix: &str, body: Value) {
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/spa/vehicles/{VEHICLE}/{suffix}")))
        .respond_with(ok(body))
        .mount(server)
        .await;
}

#[tokio::test]
async fn vehicle_list_survives_nulls_and_unusable_entries() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    let mut vehicles = fixture("vehicles");
    let list = &mut vehicles["resMsg"]["vehicles"];
    list[0]["nickname"] = Value::Null;
    list[0]["type"] = Value::Null;
    list[0]["year"] = json!(2024);
    list[0]["ccuCCS2ProtocolSupport"] = json!("0");
    let list = list.as_array_mut().unwrap();
    list.push(json!({ "vin": "NO-ID", "nickname": "Ghost" })); // unusable: no id
    list.push(json!("garbage"));
    list.push(json!({ "vehicleId": 12345, "nickname": "Second" }));
    Mock::given(method("GET"))
        .and(path("/api/v1/spa/vehicles"))
        .respond_with(ok(vehicles))
        .mount(&server)
        .await;

    let cars = client(&server, None).vehicles().await.unwrap();
    assert_eq!(cars.len(), 2, "{cars:?}");
    let first = &cars[0];
    assert_eq!(first.id, VEHICLE);
    assert_eq!(first.name, "Creta", "falls back to the model name");
    assert_eq!(first.powertrain, Powertrain::Unknown);
    assert_eq!(first.year.as_deref(), Some("2024"));
    assert!(!first.ccs2);
    assert_eq!(cars[1].id, "12345");
    assert_eq!(cars[1].name, "Second");
    assert_eq!(cars[1].model, "");
}

#[tokio::test]
async fn numeric_command_ids_are_matched_as_text() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    Mock::given(method("POST"))
        .and(path(door_path()))
        .respond_with(ok(json!({
            "retCode": "S", "resCode": "0000", "resMsg": null, "msgId": 98765
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/spa/notifications/{VEHICLE}/records")))
        .respond_with(ok(envelope(
            json!([
                "junk",
                { "recordId": null, "result": "fail" },
                { "recordId": 98765, "result": "success" },
            ]),
            "x",
        )))
        .mount(&server)
        .await;

    let client = client(&server, None);
    let id = client.lock(&car()).await.unwrap();
    assert_eq!(id.as_str(), "98765");
    let status = client
        .wait_for(
            &car(),
            &id,
            Duration::from_secs(5),
            Duration::from_millis(10),
        )
        .await
        .unwrap();
    assert_eq!(status, CommandStatus::Succeeded);
}

#[tokio::test]
async fn status_survives_null_and_string_typed_fields() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    let mut status = fixture("status_latest");
    let s = &mut status["resMsg"];
    s["fuelLevel"] = json!("63");
    s["doorLock"] = json!("1");
    s["doorOpen"] = Value::Null;
    s["dte"] = json!("garbage");
    s["time"] = json!(20_261_123_161_134_u64);
    s["battery"] = json!("x");
    s["airCtrlOn"] = json!(true);
    s["airTemp"] = json!({ "value": 5 });
    s["tirePressureLamp"] = json!([]);
    s["engine"] = Value::Null;
    s.as_object_mut().unwrap().remove("hoodOpen");
    mount_vehicle_get(&server, "status/latest", status).await;

    let status = client(&server, None).status(&car()).await.unwrap();
    assert_eq!(status.fuel_percent, Some(63.0));
    assert_eq!(status.locked, Some(true));
    assert_eq!(status.doors_open, None);
    assert_eq!(status.range, None);
    assert_eq!(status.battery_12v, None);
    assert_eq!(status.climate_on, Some(true));
    assert_eq!(status.climate_temperature, None);
    assert_eq!(status.tyre_pressure_warning, None);
    assert_eq!(status.engine_on, None);
    assert_eq!(status.hood_open, None);
    assert_eq!(
        status.updated_at.unwrap().to_string(),
        "2026-11-23T10:41:34Z"
    );
}

#[tokio::test]
async fn maintenance_and_location_survive_odd_shapes() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    let mut maintenance = fixture("maintenance");
    let items = maintenance["resMsg"]["mainList"].as_array_mut().unwrap();
    items.truncate(1);
    items.push(Value::Null);
    items.push(json!({ "itemId": 7, "enable": "Y", "items": null }));
    items.push(json!({ "enable": true, "items": { "distValue": 1 } })); // no name
    mount_vehicle_get(&server, "setting/alert/maintenance", maintenance).await;
    let mut location = fixture("location");
    location["resMsg"]["coord"] = json!({ "lat": "12.97", "lon": 77.59, "alt": null });
    location["resMsg"]["speed"] = json!("fast");
    mount_vehicle_get(&server, "location/park", location).await;

    let client = client(&server, None);
    let m = client.maintenance(&car()).await.unwrap();
    let names: Vec<&str> = m.items.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["Air Cleaner", "7"]);
    assert!(m.items[1].enabled);
    assert_eq!(m.items[1].interval, hkconnect::ServiceInterval::default());

    let l = client.location(&car()).await.unwrap().unwrap();
    assert_eq!((l.latitude, l.longitude), (12.97, 77.59));
    assert_eq!((l.altitude_m, l.speed), (None, None));
}

#[tokio::test]
async fn location_without_coordinates_is_none() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    let mut location = fixture("location");
    location["resMsg"]["coord"]["lat"] = Value::Null;
    mount_vehicle_get(&server, "location/park", location).await;
    assert_eq!(client(&server, None).location(&car()).await.unwrap(), None);
}

#[tokio::test]
async fn trips_skip_bad_entries_and_echo_string_service_ids() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    let trips_path = format!("/api/v1/spa/vehicles/{VEHICLE}/tripinfo");

    let mut month = fixture("trip_month");
    month["resMsg"]["tripDist"] = json!("486");
    let days = month["resMsg"]["tripDayList"].as_array_mut().unwrap();
    days[0]["tripCntDay"] = json!("4");
    days.push(json!({ "tripDayInMonth": null, "tripCntDay": 1 }));
    days.push(json!({ "tripDayInMonth": "20261199", "tripCntDay": 1 }));
    days.push(json!(42));
    Mock::given(method("POST"))
        .and(path(trips_path.clone()))
        .and(body_json(
            json!({ "tripPeriodType": 0, "setTripMonth": "202611" }),
        ))
        .respond_with(ok(month))
        .mount(&server)
        .await;

    let mut day = fixture("trip_day");
    let trips = day["resMsg"]["dayTripList"][0]["tripList"]
        .as_array_mut()
        .unwrap();
    trips[0]["serviceTID"] = json!("20261117080003");
    trips[0]["tripStartCoord"]["lat"] = json!("12.9716");
    trips[0]["tripEndCoord"] = json!("unknown");
    trips[1].as_object_mut().unwrap().remove("tripStartTime");
    trips.push(json!("junk"));
    trips.push(json!({ "serviceTID": null, "tripStartTime": "20261117200000" }));
    Mock::given(method("POST"))
        .and(path(trips_path))
        .and(body_json(
            json!({ "tripPeriodType": 1, "setTripDay": "20261117" }),
        ))
        .respond_with(ok(day))
        .mount(&server)
        .await;

    let mut detail = fixture("trip_detail");
    detail["resMsg"]["tripInfo"]["tripHardBreakingTime"] = json!("2");
    let route = detail["resMsg"]["tripInfo"]["tripList"]
        .as_array_mut()
        .unwrap();
    route[1]["lat"] = Value::Null;
    route[2]["tripDate"] = json!("soon");
    route.push(json!("junk"));
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/v1/spa/vehicles/{VEHICLE}/tripinfo/detail"
        )))
        .and(body_json(json!({
            "tripPeriodType": 1,
            "setTripDay": "20261117",
            "setTripStartTime": "20261117080003",
            "setServiceTID": "20261117080003",
            "tripStartTime": "20261117080003",
            "tripEndTime": "20261117081634",
        })))
        .respond_with(ok(detail))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server, None);
    let car = car();

    let month = client.trips_in_month(&car, 2026, 11).await.unwrap();
    assert_eq!(month.days.len(), 13);
    assert_eq!(month.days[0].trips, 4);
    assert_eq!(month.totals.distance_km, Some(486.0));

    let day = client
        .trips_on_day(&car, jiff::civil::date(2026, 11, 17))
        .await
        .unwrap();
    assert_eq!(day.trips.len(), 1, "{:?}", day.trips);
    let trip = &day.trips[0];
    assert_eq!(trip.id, "20261117080003");
    assert!((trip.start.unwrap().latitude - 12.9716).abs() < 1e-9);
    assert_eq!(trip.end, None);

    let detail = client.trip_detail(&car, trip).await.unwrap();
    assert_eq!(detail.hard_braking, Some(2));
    assert_eq!(detail.route.len(), 3);
    assert_eq!(detail.route[1].at, None);
}

// ----- serde: what the public types serialize, they deserialize -----

#[track_caller]
fn round_trip<T>(value: &T)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(value).unwrap();
    let back: T = serde_json::from_str(&json).unwrap_or_else(|e| {
        panic!(
            "{} does not read back: {e}\n{json}",
            std::any::type_name::<T>()
        )
    });
    assert_eq!(&back, value, "{json}");
}

#[tokio::test]
async fn every_public_data_type_round_trips_through_serde() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    mount_vehicles(&server).await;
    let mut status = fixture("status_latest");
    status["resMsg"]["airCtrlOn"] = json!(true);
    status["resMsg"]["airTemp"]["value"] = json!("10H");
    status["resMsg"]["battery"]["batSoc"] = json!(87);
    mount_vehicle_get(&server, "status/latest", status).await;
    mount_vehicle_get(&server, "location/park", fixture("location")).await;
    mount_vehicle_get(&server, "profile", fixture("profile")).await;
    mount_vehicle_get(&server, "setting/alert/maintenance", fixture("maintenance")).await;
    let trips_path = format!("/api/v1/spa/vehicles/{VEHICLE}/tripinfo");
    Mock::given(method("POST"))
        .and(path(trips_path.clone()))
        .and(body_string_contains("setTripMonth"))
        .respond_with(ok(fixture("trip_month")))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(trips_path))
        .and(body_string_contains("setTripDay"))
        .respond_with(ok(fixture("trip_day")))
        .mount(&server)
        .await;
    // Only reachable with the identifiers a restored trip carries.
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/v1/spa/vehicles/{VEHICLE}/tripinfo/detail"
        )))
        .and(body_json(json!({
            "tripPeriodType": 1,
            "setTripDay": "20261117",
            "setTripStartTime": "20261117054442",
            "setServiceTID": 20_261_117_054_442_u64,
            "tripStartTime": "20261117054442",
            "tripEndTime": "20261117060156",
        })))
        .respond_with(ok(fixture("trip_detail")))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server, None);
    let cars = client.vehicles().await.unwrap();
    round_trip(&cars);
    let car = &cars[0];
    let car_json = serde_json::to_value(car).unwrap();
    assert_eq!(car_json["id"], VEHICLE, "own field names, not the API's");
    assert_eq!(car_json["powertrain"], "petrol");

    let status = client.status(car).await.unwrap();
    assert_eq!(status.climate_temperature, Celsius::new(22.0).ok());
    round_trip(&status);
    round_trip(&status.doors_open.unwrap());
    round_trip(&status.range.unwrap());
    round_trip(&status.battery_12v.unwrap());
    round_trip(&status.climate_temperature.unwrap());

    let location = client.location(car).await.unwrap().unwrap();
    round_trip(&location);
    round_trip(&client.capabilities(car).await.unwrap());
    let maintenance = client.maintenance(car).await.unwrap();
    round_trip(&maintenance);
    round_trip(&maintenance.items[0]);
    round_trip(&maintenance.items[0].interval);

    let month = client.trips_in_month(car, 2026, 11).await.unwrap();
    round_trip(&month);
    round_trip(&month.totals);
    round_trip(&month.days[0]);
    let day = client
        .trips_on_day(car, jiff::civil::date(2026, 11, 17))
        .await
        .unwrap();
    round_trip(&day);
    let trip = &day.trips[0];
    round_trip(trip);
    round_trip(&trip.start.unwrap());

    // A trip stored as JSON and read back still fetches its detail.
    let stored = serde_json::to_string(trip).unwrap();
    assert!(stored.contains("_detail_ref"), "{stored}");
    let restored: hkconnect::Trip = serde_json::from_str(&stored).unwrap();
    let detail = client.trip_detail(car, &restored).await.unwrap();
    round_trip(&detail);
    round_trip(&detail.route[0]);

    round_trip(&client.session().await.unwrap());
}

#[test]
fn ids_enums_and_inputs_round_trip_through_serde() {
    round_trip(&CommandId::new("cmd-1"));
    assert_eq!(
        serde_json::to_string(&CommandId::new("cmd-1")).unwrap(),
        r#""cmd-1""#
    );
    for s in [
        CommandStatus::Pending,
        CommandStatus::Succeeded,
        CommandStatus::Failed,
        CommandStatus::NoResponse,
        CommandStatus::Unknown,
    ] {
        round_trip(&s);
    }
    for p in [
        Powertrain::Petrol,
        Powertrain::Electric,
        Powertrain::PlugInHybrid,
        Powertrain::Hybrid,
        Powertrain::Unknown,
    ] {
        round_trip(&p);
    }
    for u in [hkconnect::DistanceUnit::Km, hkconnect::DistanceUnit::Miles] {
        round_trip(&u);
    }
    round_trip(
        &Destination::new("Office", 12.9716, 77.5946)
            .unwrap()
            .with_address("MG Road"),
    );
}

// ----- single-flight login and PIN handling -----

/// Mount a login whose sign-in step rejects the password; each step may be
/// hit exactly `attempts` times.
async fn mount_rejected_login(server: &MockServer, attempts: u64) {
    Mock::given(method("POST"))
        .and(path("/api/v1/spa/notifications/register"))
        .respond_with(ok(fixture("register")))
        .expect(attempts)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/user/oauth2/authorize"))
        .respond_with(ResponseTemplate::new(200))
        .expect(attempts)
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/user/signin"))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(json!({ "errMsg": "wrong password" })),
        )
        .expect(attempts)
        .mount(server)
        .await;
}

#[tokio::test]
async fn concurrent_reads_share_one_failed_login() {
    let server = MockServer::start().await;
    mount_rejected_login(&server, 1).await; // verified on drop: one sign-in
    let client = client(&server, None);
    let car = car();
    let (a, b, c, d) = tokio::join!(
        client.vehicles(),
        client.capabilities(&car),
        client.maintenance(&car),
        client.location(&car),
    );
    for err in [
        a.unwrap_err(),
        b.unwrap_err(),
        c.unwrap_err(),
        d.unwrap_err(),
    ] {
        assert!(matches!(err, Error::Authentication(_)), "{err:?}");
    }
}

#[tokio::test]
async fn a_call_after_a_reported_login_failure_tries_again() {
    let server = MockServer::start().await;
    mount_rejected_login(&server, 2).await;
    let client = client(&server, None);
    assert!(client.vehicles().await.is_err());
    assert!(client.vehicles().await.is_err());
}

async fn mount_engine_stop(server: &MockServer, expected: u64) {
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/v2/spa/vehicles/{VEHICLE}/control/engine"
        )))
        .respond_with(ok(envelope(Value::Null, "cmd-3")))
        .expect(expected)
        .mount(server)
        .await;
}

#[tokio::test]
async fn a_wrong_pin_is_submitted_once_and_never_again() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/user/pin"))
        .respond_with(ok(json!({ "errCode": "4011", "errMsg": "Invalid PIN" })))
        .expect(1)
        .mount(&server)
        .await;
    mount_engine_stop(&server, 0).await;

    let client = client(&server, Some("0000"));
    let car = car();
    let (a, b) = tokio::join!(client.stop_climate(&car), client.valet_mode(&car, true));
    for err in [a.unwrap_err(), b.unwrap_err()] {
        assert!(matches!(err, Error::InvalidPin), "{err:?}");
        assert!(!err.may_have_been_received());
    }
    // Later commands fail at once, without asking the server again.
    let err = client.stop_climate(&car).await.unwrap_err();
    assert!(matches!(err, Error::InvalidPin), "{err:?}");
    assert!(matches!(
        client.hazard_lights(&car, false).await,
        Err(Error::InvalidPin)
    ));
    // Commands that don't need the PIN are unaffected.
    Mock::given(method("POST"))
        .and(path(door_path()))
        .respond_with(ok(envelope(Value::Null, "cmd-1")))
        .expect(1)
        .mount(&server)
        .await;
    client.lock(&car).await.unwrap();
}

#[tokio::test]
async fn a_pin_refused_with_http_400_is_invalid_pin_and_not_sent() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/user/pin"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({ "errMsg": "Invalid PIN" })))
        .expect(1)
        .mount(&server)
        .await;
    mount_engine_stop(&server, 0).await;

    let client = client(&server, Some("0000"));
    let err = client.stop_climate(&car()).await.unwrap_err();
    assert!(matches!(err, Error::InvalidPin), "{err:?}");
    assert!(!err.may_have_been_received());
    assert!(matches!(
        client.stop_climate(&car()).await,
        Err(Error::InvalidPin)
    ));
}

#[tokio::test]
async fn a_pin_refused_with_401_right_after_login_is_not_resubmitted() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await; // no second login to "recover"
    Mock::given(method("PUT"))
        .and(path("/api/v1/user/pin"))
        .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
        .expect(1)
        .mount(&server)
        .await;
    mount_engine_stop(&server, 0).await;

    let client = client(&server, Some("0000"));
    let err = client.stop_climate(&car()).await.unwrap_err();
    assert!(matches!(err, Error::InvalidPin), "{err:?}");
    assert!(!err.may_have_been_received());
    assert!(matches!(
        client.stop_climate(&car()).await,
        Err(Error::InvalidPin)
    ));
}

#[tokio::test]
async fn a_pin_step_outage_is_not_taken_as_a_wrong_pin() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/user/pin"))
        .respond_with(Sequence::new(vec![
            ResponseTemplate::new(503).set_body_string("down"),
            ok(json!({ "controlToken": "ctl-1", "expiresTime": "600" })),
        ]))
        .expect(2)
        .mount(&server)
        .await;
    mount_engine_stop(&server, 1).await;

    let client = client(&server, Some("1234"));
    // An outage is not a PIN rejection: it is reported (conservatively, by
    // variant) and the next call asks again, with a string-typed lifetime.
    let err = client.stop_climate(&car()).await.unwrap_err();
    assert!(matches!(err, Error::ServiceUnavailable), "{err:?}");
    // Documented as conservative: the variant alone cannot show it was the
    // PIN step that failed.
    assert!(err.may_have_been_received());
    assert_eq!(client.stop_climate(&car()).await.unwrap().as_str(), "cmd-3");
}

#[tokio::test]
async fn a_command_answered_with_an_auth_looking_2xx_error_may_have_been_received() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await; // no re-login, no resend
    Mock::given(method("POST"))
        .and(path(door_path()))
        .respond_with(ok(
            json!({ "error": "Key not authorized: Token is expired" }),
        ))
        .expect(1)
        .mount(&server)
        .await;

    let err = client(&server, None).unlock(&car()).await.unwrap_err();
    assert!(matches!(err, Error::UnexpectedResponse(_)), "{err:?}");
    assert!(err.may_have_been_received());
}

/// A server that accepts connections, reads the request, and hangs up
/// without answering: a network error that is not a connection failure.
fn hang_up_server() -> hkconnect::url::Url {
    use std::io::Read as _;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0; 4096];
            let _ = stream.read(&mut buf);
            drop(stream);
        }
    });
    format!("http://{addr}").parse().unwrap()
}

fn fresh_session() -> hkconnect::Session {
    serde_json::from_value(json!({
        "access_token": "Bearer access-1",
        "refresh_token": "refresh-1",
        "expires_at": "2100-01-01T00:00:00Z",
        "device_id": DEVICE,
    }))
    .unwrap()
}

#[tokio::test]
async fn network_errors_say_whether_the_command_itself_was_sent() {
    let base = hang_up_server();
    let config = Config::india(Brand::Hyundai).with_base_url(base);

    // Failing while logging in: the unlock was never sent.
    let client = Client::new(config.clone(), credentials(None)).unwrap();
    let err = client.unlock(&car()).await.unwrap_err();
    let Error::Http(http) = &err else {
        panic!("{err:?}")
    };
    assert!(!http.is_connect());
    assert!(!err.may_have_been_received(), "{err:?}");

    // Failing while sending the unlock itself: it may have arrived.
    let client = Client::new(config, credentials(None))
        .unwrap()
        .with_session(fresh_session());
    let err = client.unlock(&car()).await.unwrap_err();
    assert!(matches!(err, Error::Http(_)), "{err:?}");
    assert!(err.may_have_been_received(), "{err:?}");

    // The cause is kept, and the URL (with the vehicle id) is not shown.
    let chain = std::iter::successors(Some(&err as &dyn std::error::Error), |e| e.source())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    assert!(chain.len() >= 3, "{chain:?}");
    assert!(!chain.join(": ").contains(VEHICLE), "{chain:?}");
}

// ----- hostile input -----

#[tokio::test]
async fn hostile_vehicle_ids_never_reach_the_server() {
    let server = MockServer::start().await;
    let client = client(&server, Some("1234")).with_session(fresh_session());
    for id in [
        "../../../user/pin?",
        "a/b",
        "x?y",
        "x#y",
        "%2e%2e",
        "a\\b",
        "..",
        ".",
        "",
    ] {
        let car: hkconnect::Vehicle = serde_json::from_value(json!({
            "id": id, "vin": "", "name": "x", "model": "", "year": null,
            "powertrain": "unknown", "ccs2": false,
        }))
        .unwrap();
        let trip_day = jiff::civil::date(2026, 11, 17);
        let errors = [
            client.status(&car).await.err(),
            client.refresh_status(&car).await.err(),
            client.location(&car).await.err(),
            client.maintenance(&car).await.err(),
            client.capabilities(&car).await.err(),
            client.trips_in_month(&car, 2026, 11).await.err(),
            client.trips_on_day(&car, trip_day).await.err(),
            client.lock(&car).await.err(),
            client.unlock(&car).await.err(),
            client
                .start_climate(&car, &ClimateOptions::default())
                .await
                .err(),
            client.stop_climate(&car).await.err(),
            client.hazard_lights(&car, true).await.err(),
            client.valet_mode(&car, true).await.err(),
            client
                .send_destination(&car, &Destination::new("x", 0.0, 0.0).unwrap())
                .await
                .err(),
            client
                .command_status(&car, &CommandId::new("1"))
                .await
                .err(),
        ];
        for err in errors {
            let err = err.unwrap();
            assert!(matches!(err, Error::InvalidVehicleId), "{id:?}: {err:?}");
            assert!(!err.may_have_been_received());
        }
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

// ----- more leniency -----

#[tokio::test]
async fn string_token_lifetimes_are_accepted() {
    let server = MockServer::start().await;
    mount_login_with_token(
        &server,
        1,
        ok(json!({
            "access_token": "access-1", "token_type": "Bearer",
            "refresh_token": "refresh-1", "expires_in": "3600"
        })),
    )
    .await;
    Mock::given(method("PUT"))
        .and(path("/api/v1/user/pin"))
        .respond_with(ok(json!({ "controlToken": "ctl-1", "expiresTime": "600" })))
        .expect(1)
        .mount(&server)
        .await;
    mount_engine_stop(&server, 2).await;

    let client = client(&server, Some("1234"));
    let session = client.login().await.unwrap();
    let hour = jiff::SignedDuration::from_secs(3500);
    assert!(session.expires_at() > jiff::Timestamp::now() + hour);
    client.stop_climate(&car()).await.unwrap();
    client.stop_climate(&car()).await.unwrap(); // token cached
}

#[tokio::test]
async fn null_or_string_payloads_read_as_empty() {
    let server = MockServer::start().await;
    mount_login(&server, 1).await;
    let success = |res_msg: Value| envelope(res_msg, "m");
    mount_vehicle_get(&server, "status/latest", success(Value::Null)).await;
    mount_vehicle_get(&server, "status", success(json!("nothing"))).await;
    mount_vehicle_get(&server, "setting/alert/maintenance", success(json!("x"))).await;
    mount_vehicle_get(&server, "location/park", success(json!("x"))).await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/api/v1/spa/vehicles/{VEHICLE}/tripinfo/detail"
        )))
        .respond_with(Sequence::new(vec![
            ok(success(json!({}))),
            ok(success(json!({ "tripInfo": null }))),
            ok(success(Value::Null)),
        ]))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/api/v1/spa/vehicles/{VEHICLE}/tripinfo")))
        .respond_with(ok(success(Value::Null)))
        .mount(&server)
        .await;

    let client = client(&server, None);
    let car = car();
    assert_eq!(
        client.status(&car).await.unwrap(),
        hkconnect::VehicleStatus::default()
    );
    assert_eq!(
        client.refresh_status(&car).await.unwrap(),
        hkconnect::VehicleStatus::default()
    );
    let m = client.maintenance(&car).await.unwrap();
    assert_eq!((m.odometer_km, m.items.len()), (None, 0));
    assert_eq!(client.location(&car).await.unwrap(), None);

    let month = client.trips_in_month(&car, 2026, 11).await.unwrap();
    assert!(month.days.is_empty());
    let day = client
        .trips_on_day(&car, jiff::civil::date(2026, 11, 17))
        .await
        .unwrap();
    assert!(day.trips.is_empty());

    let trip: hkconnect::Trip = serde_json::from_value(json!({
        "id": "1", "started_at": null, "ended_at": null, "start": null, "end": null,
        "_detail_ref": { "service_tid": 1, "start": "20261117054442", "end": "20261117060156" },
    }))
    .unwrap();
    for _ in 0..3 {
        let detail = client.trip_detail(&car, &trip).await.unwrap();
        assert!(detail.route.is_empty() && detail.started_at.is_none());
    }
}

#[tokio::test]
async fn numeric_res_code_7501_triggers_a_login() {
    let server = MockServer::start().await;
    mount_login(&server, 2).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/spa/vehicles"))
        .respond_with(Sequence::new(vec![
            ok(json!({ "retCode": "F", "resCode": 7501, "resMsg": "expired" })),
            ok(fixture("vehicles")),
        ]))
        .expect(2)
        .mount(&server)
        .await;

    let client = client(&server, None);
    client.login().await.unwrap();
    assert_eq!(client.vehicles().await.unwrap().len(), 1);
}
