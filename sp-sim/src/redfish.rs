// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Live sensor ingestion from a real BMC via Redfish.
//!
//! When a `real_sensor_source` is present in the sp-sim config, we
//! periodically poll the BMC (e.g. a Dell iDRAC) for real readings
//! (temperatures, fan speeds, power) and push them into the simulated SPs'
//! sensors via [`crate::server::Command::UpdateSensors`]. The simulated
//! sensors keep their configured names and kinds; only their `last_data`
//! values are replaced, so the entire MGS -> oximeter metrics chain keeps
//! working unchanged, but with real values.
//!
//! This code is deliberately host-agnostic: the same binary can either run
//! in-band (in the switch zone, next to sp-sim) or on a standalone "SP
//! node" with only a config change.

use crate::config::RealSensorSource;
use crate::server::Command;
use crate::server::SensorDataPoint;
use anyhow::anyhow;
use anyhow::Result;
use gateway_messages::measurement::MeasurementKind;
use gateway_messages::SpComponent;
use slog::Logger;
use slog::debug;
use slog::error;
use slog::info;
use slog::warn;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

/// Component (on the simulated PSC) that carries the real host telemetry.
/// Deliberately not a PSU slot name, so it is ignored by Nexus's PSU
/// inventory (which quietly skips components that don't look like PSUs).
const MON0: &str = "MON0";

/// Spawns the live-sensor poller.
///
/// `targets` are the command senders of the simulated SPs whose sensors
/// should receive the real values (currently the PSCs).
pub(crate) fn spawn(
    config: RealSensorSource,
    log: Logger,
    targets: Vec<UnboundedSender<Command>>,
) {
    tokio::spawn(async move {
        let poll_interval =
            Duration::from_secs(config.poll_interval_secs.unwrap_or(30).max(5));
        let insecure_tls = config.insecure_tls.unwrap_or(true);
        let chassis = config
            .chassis_id
            .unwrap_or_else(|| "System.Embedded.1".to_string());

        let password = match (&config.password, &config.password_file) {
            (Some(password), _) => password.clone(),
            (None, Some(path)) => match std::fs::read_to_string(path) {
                Ok(contents) => contents.trim().to_string(),
                Err(err) => {
                    error!(log, "redfish: cannot read password file {path}"; "error" => %err);
                    return;
                }
            },
            (None, None) => {
                error!(log, "redfish: no password or password_file configured");
                return;
            }
        };

        let client = match reqwest::Client::builder()
            .danger_accept_invalid_certs(insecure_tls)
            .timeout(Duration::from_secs(30))
            .build()
        {
            Ok(client) => client,
            Err(err) => {
                error!(log, "redfish: failed to build HTTP client"; "error" => %err);
                return;
            }
        };

        let mut bmc = Redfish {
            log: log.clone(),
            client,
            base: config.base_url.trim_end_matches('/').to_string(),
            chassis,
            username: config.username,
            password,
            token: None,
        };

        info!(
            log,
            "redfish: live sensor source started";
            "base_url" => %config.base_url,
            "poll_interval_secs" => poll_interval.as_secs(),
            "targets" => targets.len(),
        );

        loop {
            match bmc.read_once().await {
                Ok(updates) if updates.is_empty() => {
                    warn!(log, "redfish: no mappable readings returned");
                }
                Ok(updates) => {
                    let len = updates.len();
                    for target in &targets {
                        let _ =
                            target.send(Command::UpdateSensors(updates.clone()));
                    }
                    debug!(log, "redfish: pushed {len} live sensor values");
                }
                Err(err) => {
                    error!(log, "redfish: poll failed"; "error" => %err);
                }
            }
            tokio::time::sleep(poll_interval).await;
        }
    });
}

/// A minimal Redfish client.
struct Redfish {
    log: Logger,
    client: reqwest::Client,
    base: String,
    chassis: String,
    username: String,
    password: String,
    token: Option<String>,
}

impl Redfish {
    async fn read_once(&mut self) -> Result<Vec<SensorDataPoint>> {
        let thermal = self.get_json("Thermal").await?;
        let power = self.get_json("Power").await?;
        Ok(map_readings(thermal, power))
    }

    /// GETs a Chassis sub-resource as JSON, authenticating as needed.
    async fn get_json(&mut self, resource: &str) -> Result<serde_json::Value> {
        let url = format!(
            "{}/redfish/v1/Chassis/{}/{}",
            self.base, self.chassis, resource
        );
        for _ in 0..2 {
            let mut request = self.client.get(&url);
            request = match &self.token {
                Some(token) => request.header("X-Auth-Token", token),
                None => request.basic_auth(&self.username, Some(&self.password)),
            };
            let response = request.send().await?;
            if response.status().as_u16() == 401 {
                // Either our session token expired or this BMC doesn't
                // accept basic auth. Establish a real session, then retry.
                debug!(self.log, "redfish: 401, establishing session");
                self.login().await?;
                continue;
            }
            let response = response.error_for_status()?;
            return Ok(response.json().await?);
        }
        Err(anyhow!("redfish: authentication to {} failed", self.base))
    }

    /// Creates a Redfish session, saving the X-Auth-Token for later use.
    async fn login(&mut self) -> Result<()> {
        let url = format!("{}/redfish/v1/Sessions", self.base);
        let body = serde_json::json!({
            "UserName": self.username,
            "Password": self.password,
        });
        let response = self.client.post(&url).json(&body).send().await?;
        let status = response.status();
        if let Some(token) = response
            .headers()
            .get("X-Auth-Token")
            .and_then(|value| value.to_str().ok())
        {
            self.token = Some(token.to_string());
            debug!(self.log, "redfish: session established");
            Ok(())
        } else {
            Err(anyhow!("redfish: session login failed: {status}"))
        }
    }
}

/// Maps one round of BMC readings onto the simulated sensors.
///
/// Real readings with no PSU analog land on `MON0`; PSU input voltage is
/// additionally pushed into the corresponding synthetic `PSU{n}` rail
/// entries (`V54_PSU{n}`/`V12_PSU{n}` InputVoltage), where one exists.
///
/// Updates for (component, name, kind) triples that aren't configured are
/// silently dropped by `Sensors::apply_updates`, so a BMC that returns more
/// hardware than the config models is harmless.
fn map_readings(
    thermal: serde_json::Value,
    power: serde_json::Value,
) -> Vec<SensorDataPoint> {
    let mon0 =
        SpComponent::try_from(MON0).expect("MON0 must be a valid component");
    let mut out = Vec::new();

    // Temperatures.
    if let Some(temps) = thermal.get("Temperatures").and_then(|v| v.as_array())
    {
        for temp in temps {
            let Some(name) = temp.get("Name").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(value) =
                temp.get("ReadingCelsius").and_then(|v| v.as_f64())
            else {
                continue;
            };
            let sim_name = match name {
                "System Board Inlet Temp" => "bmc_inlet_temp",
                "CPU1 Temp" => "bmc_cpu1_temp",
                "CPU2 Temp" => "bmc_cpu2_temp",
                _ => continue,
            };
            out.push(point(
                mon0.clone(),
                sim_name,
                MeasurementKind::Temperature,
                value,
            ));
        }
    }

    // Fans ("System Board Fan3A" -> "bmc_fan3a").
    if let Some(fans) = thermal.get("Fans").and_then(|v| v.as_array()) {
        for fan in fans {
            let Some(name) = fan.get("FanName").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(rpm) = fan.get("Reading").and_then(|v| v.as_f64()) else {
                continue;
            };
            let Some(suffix) = name.strip_prefix("System Board Fan") else {
                continue;
            };
            // Guard against non-RPM fan readings.
            if !(0.0..100_000.0).contains(&rpm) {
                continue;
            }
            let sim_name = format!("bmc_fan{}", suffix.to_lowercase());
            out.push(point(mon0.clone(), &sim_name, MeasurementKind::Speed, rpm));
        }
    }

    // Total power draw.
    if let Some(watts) = power
        .get("PowerControl")
        .and_then(|v| v.as_array())
        .and_then(|controls| controls.first())
        .and_then(|control| control.get("PowerConsumedWatts"))
        .and_then(|v| v.as_f64())
    {
        out.push(point(
            mon0.clone(),
            "bmc_total_power",
            MeasurementKind::Power,
            watts,
        ));
    }

    // Power supplies.
    if let Some(psus) = power.get("PowerSupplies").and_then(|v| v.as_array())
    {
        for (i, psu) in psus.iter().enumerate() {
            let index = psu
                .get("MemberId")
                .and_then(|v| v.as_str())
                .and_then(|id| id.rsplit('.').next())
                .and_then(|slot| slot.parse::<usize>().ok())
                .map(|slot| slot.saturating_sub(1))
                .unwrap_or(i);
            let psun = format!("PSU{index}");
            let Ok(sim_psu) = SpComponent::try_from(psun.as_str()) else {
                continue;
            };

            if let Some(volts) =
                psu.get("LineInputVoltage").and_then(|v| v.as_f64())
            {
                out.push(point(
                    mon0.clone(),
                    &format!("bmc_psu{}_input", index + 1),
                    MeasurementKind::Voltage,
                    volts,
                ));
                // Also refresh the synthetic rail entries, if configured.
                for rail in ["V54", "V12"] {
                    out.push(point(
                        sim_psu.clone(),
                        &format!("{rail}_{psun}"),
                        MeasurementKind::InputVoltage,
                        volts,
                    ));
                }
            }
            if let Some(watts) =
                psu.get("PowerInputWatts").and_then(|v| v.as_f64())
            {
                out.push(point(
                    mon0.clone(),
                    &format!("bmc_psu{}_in_power", index + 1),
                    MeasurementKind::Power,
                    watts,
                ));
            }
            if let Some(watts) =
                psu.get("LastPowerOutputWatts").and_then(|v| v.as_f64())
            {
                out.push(point(
                    mon0.clone(),
                    &format!("bmc_psu{}_out_power", index + 1),
                    MeasurementKind::Power,
                    watts,
                ));
            }
        }
    }

    out
}

fn point(
    component: SpComponent,
    name: &str,
    kind: MeasurementKind,
    value: f64,
) -> SensorDataPoint {
    SensorDataPoint {
        component,
        name: name.to_string(),
        kind,
        value: value as f32,
    }
}
