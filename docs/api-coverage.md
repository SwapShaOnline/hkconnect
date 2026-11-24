# API coverage

Every endpoint the reference implementation
([hyundai_kia_connect_api](https://github.com/Hyundai-Kia-Connect/hyundai_kia_connect_api))
uses for India, and its status in hkconnect.

All paths are relative to:

```text
https://prd.in-ccapi.{hyundai|kia}.connected-car.io:8080
```

`v1` and `v2` below are short for `/api/v1/spa` and `/api/v2/spa`, and
`{id}` is the vehicle id.

Status values:

- **Implemented**: implemented, tested against a fake server, and verified
  on a real car (2024 Hyundai Creta, petrol, automatic, non-CCS2).
- **Implemented, unverified**: implemented to match the reference
  implementation and tested against a fake server, but not yet confirmed on
  a real car. Treat as experimental.
- **Not implemented**: not available in hkconnect.

Auth column:

- **Access token**: the normal OAuth access token.
- **Control token**: a short-lived token obtained by exchanging the PIN
  (`PUT /api/v1/user/pin`). Sent in both `Authorization` and
  `AuthorizationCCSP` headers.
- **Stamp**: the app `Stamp` header (no user token).

## Authentication

| Capability | Method and path | Auth | hkconnect status | Notes |
|---|---|---|---|---|
| Register device | `POST /api/v1/spa/notifications/register` | Stamp | Implemented | Returns the `deviceId` used in later headers. Repeated automatically after error 4002. |
| Start OAuth flow | `GET /api/v1/user/oauth2/authorize` | None | Implemented | Sets the session cookie used by sign-in. |
| Sign in | `POST /api/v1/user/signin` | Cookie | Implemented | `{email, password}`; returns a redirect URL containing the authorization code. |
| Token exchange | `POST /api/v1/user/oauth2/token` (`grant_type=authorization_code`) | Basic (app), Stamp | Implemented | Access token (about 1 hour) and refresh token. |
| Token refresh | `POST /api/v1/user/oauth2/token` (`grant_type=refresh_token`) | Basic (app), Stamp | Implemented | Automatic; falls back to a full login if refresh fails. |
| PIN to control token | `PUT /api/v1/user/pin?token=` | Access token | Implemented | `{deviceId, pin}`; returns `controlToken` and `expiresTime`. Cached until shortly before expiry. |
| Set language | `POST /api/v1/user/language` | Access token | Not implemented | Not needed for any supported call. |

## Reads

| Capability | Method and path | Auth | hkconnect status | Notes |
|---|---|---|---|---|
| List vehicles | `GET v1/vehicles` | Access token | Implemented | `Client::vehicles`, `hkc vehicles`. Includes the CCS2 flag. |
| Cached status | `GET v1/vehicles/{id}/status/latest` | Access token | Implemented | `Client::status`, `hkc status`. Does not wake the car; can be hours old. |
| Refreshed status | `GET v1/vehicles/{id}/status` | Access token | Implemented | `Client::refresh_status`, `hkc status --refresh`. Wakes the car and uses its 12V battery. |
| Parking location | `GET v1/vehicles/{id}/location/park` | Access token | Implemented | `Client::location`, `hkc location`. |
| Capabilities | `GET v1/vehicles/{id}/profile` | Access token | Implemented | `Client::capabilities`, `hkc info`. Which remote features the car supports. |
| Odometer and service intervals | `GET v1/vehicles/{id}/setting/alert/maintenance` | Access token | Implemented | `Client::maintenance`, `hkc info`. |
| Command results | `GET v1/notifications/{id}/records` | Access token | Implemented | `Client::command_status`, `Client::wait_for`, `hkc check`. Result is `success`, `fail`, `non-response`, or pending. |
| Trips by month | `POST v1/vehicles/{id}/tripinfo` with `{tripPeriodType: 0, setTripMonth: "YYYYMM"}` | Access token | Implemented | Monthly totals and days with trips. `Client::trips_in_month`, `hkc trips`. |
| Trips by day | `POST v1/vehicles/{id}/tripinfo` with `{tripPeriodType: 1, setTripDay: "YYYYMMDD"}` | Access token | Implemented | Trips on one day with start and end points. `Client::trips_on_day`, `hkc trips`. |
| Trip detail | `POST v1/vehicles/{id}/tripinfo/detail` | Access token | Implemented | `Client::trip_detail`, `hkc trips`. Route points plus harsh braking, rapid acceleration and speeding counts. The reference implementation skips this for Hyundai non-EVs, but it works on the Creta. Contains GPS routes: personal data. |
| EV driving history | `POST v1/vehicles/{id}/drvhistory` | Access token | Not implemented | EV energy statistics; returns empty data for petrol cars. |
| CCS2 status | `GET v1/vehicles/{id}/ccs2/carstatus` and `.../ccs2/carstatus/latest` | Access token | Not implemented | Status for CCS2 cars. hkconnect rejects CCS2 cars with a clear error. |

## Commands

Commands return a `msgId` immediately; the car's result appears later in the
command results endpoint, usually after 10 to 40 seconds.

| Capability | Method and path | Auth | hkconnect status | Notes |
|---|---|---|---|---|
| Lock / unlock (non-CCS2) | `POST v1/vehicles/{id}/control/door` | Access token | Lock: implemented, verified (2024 Creta, 2026-11-23). Unlock: implemented, verified (2024 Creta, 2026-11-24; confirmed in about 12 s) | `{action: "close" \| "open", deviceId}`. `Client::lock`, `Client::unlock`, `hkc lock`, `hkc unlock`. |
| Lock / unlock (CCS2) | `POST v2/vehicles/{id}/ccs2/control/door` | Control token | Not implemented | hkconnect rejects every command for CCS2 cars with a clear error. |
| Remote start with climate | `POST v1/vehicles/{id}/control/engine` | Access token | Implemented, verified (2024 Creta, 2026-11-24) | `{action: "start", hvacType: 1, options: {defrost, heating1, igniOnDuration}, tempCode, unit: "C"}`. 14 to 29.5 °C in 0.5 steps (`tempCode` is the step index in hex plus `H`), 1 to 10 minutes. `Client::start_climate`, `hkc start`. |
| Stop remote start | `POST v2/vehicles/{id}/control/engine` | Control token | Implemented, verified (2024 Creta, 2026-11-24) | `{action: "stop"}`. `Client::stop_climate`, `hkc stop`. Works with the control token on a non-CCS2 car. |
| Hazard lights | `POST v2/vehicles/{id}/ccs2/control/light` | Control token | Implemented, unverified | Experimental on non-CCS2 cars: the endpoint is under `ccs2/`. `Client::hazard_lights`, `hkc hazard`. |
| Hazard lights and horn | `POST v2/vehicles/{id}/ccs2/control/hornlight` | Control token | Implemented, unverified | As above. `hkc hazard --horn`. |
| Valet mode | `POST v2/vehicles/{id}/control/valet` | Control token | Implemented, works (2024 Creta, 2026-11-24); result not reported | `{action: "activate" \| "deactivate"}`. `Client::valet_mode`, `hkc valet`. On and off both took effect on the car, but the command never appears in `notifications/{id}/records`, so `wait_for` ends with `Unknown`. |
| Send destination | `POST v2/vehicles/{id}/location/routes` | Control token | Implemented, verified (2024 Creta, 2026-11-24; confirmed in about 24 s) | `{deviceID, poiInfoList}`. Sends a point of interest to the car's navigation. `Client::send_destination`, `hkc navigate`. |
| Windows | `POST v2/vehicles/{id}/control/windowcurtain` | Control token | Not implemented | Not in the Creta's capability profile. |

## EV charging

None of these are implemented. They apply to electric and plug-in vehicles
only.

| Capability | Method and path | Auth | hkconnect status | Notes |
|---|---|---|---|---|
| Start / stop charging | `POST v1/vehicles/{id}/control/charge`, `POST v2/vehicles/{id}/control/charge` | Access / control token | Not implemented | |
| Charge limits | `POST v1/vehicles/{id}/charge/target` | Access token | Not implemented | |
| Charging current | `POST v2/vehicles/{id}/ccs2/charge/chargingcurrent` | Control token | Not implemented | CCS2 only. |
| Discharge limit | `POST v2/vehicles/{id}/ccs2/charge/dischargelimit` | Control token | Not implemented | CCS2 only. |
| Charge port door | `POST v2/vehicles/{id}/control/portdoor` | Control token | Not implemented | |
| Reservations and schedules | `v2/vehicles/{id}/ccs2/reservation/...` and schedule endpoints | Control token | Not implemented | Charging and climate schedules. |

## Error codes

Failed responses carry `retCode: "F"` and a `resCode`. hkconnect maps the
known codes to `hkconnect::Error` variants:

| `resCode` | Meaning | `Error` variant | hkconnect behaviour |
|---|---|---|---|
| 7501 | Authentication failed or session expired | `Authentication` | Logs in again and retries once. |
| 4002 | Invalid device id | `InvalidDeviceId` | Re-registers the device and retries once. |
| 4004 | Duplicate request (a previous command is still running) | `DuplicateRequest` | Returned; try again shortly. |
| 4005 | Command not supported by this vehicle | `UnsupportedCommand` | Returned. |
| 4081, 9999 | The car did not respond in time | `CarTimeout` | Returned. |
| 5031 | Remote service temporarily unavailable | `ServiceUnavailable` | Returned. |
| 5091 | Rate limited | `RateLimited` | Returned. Space out requests. |
| 5921 | No data for this request | `NoData` | Returned. |
| other | Unknown | `Api { code, message }` | Returned with the server's code and message. |

## Beyond the reference implementation

The official myHyundai / Bluelink India and Kia Connect apps likely offer
features that no open-source client covers, for example:

- Geofence alerts
- Speed and curfew (time fence) alerts
- Valet alerts
- Remote seat ventilation and other climate options
- Other notification and alert settings

Their endpoints are not documented anywhere public. Discovering them requires
capturing the official app's traffic with your own account, then following
the process in [CONTRIBUTING.md](../CONTRIBUTING.md#adding-support-for-a-new-endpoint).
This is future work; contributions are welcome.
