use anyhow::{Result, anyhow};
use chrono::{Local, TimeZone};
use futures::stream;
use influxdb2::Client as InfluxClient;
use influxdb2::api::write::TimestampPrecision;
use influxdb2::models::{DataPoint, Query};
use influxdb2_structmap::value::Value as InfluxValue;
use macro_factor_api::client::MacroFactorClient;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::PathBuf;

/// Firebase Web API key embedded in the MacroFactor app (not a secret).
const FIREBASE_WEB_API_KEY: &str = "AIzaSyA17Uwy37irVEQSwz6PIyX3wnkHrDBeleA";

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env when running locally; ignore if the file is absent (e.g. in Docker).
    dotenvy::dotenv().ok();

    // --- InfluxDB config ---
    let influx_url =
        env::var("INFLUX_URL").map_err(|_| anyhow!("INFLUX_URL env var is required"))?;
    let influx_org =
        env::var("INFLUX_ORG").map_err(|_| anyhow!("INFLUX_ORG env var is required"))?;
    let influx_token =
        env::var("INFLUX_TOKEN").map_err(|_| anyhow!("INFLUX_TOKEN env var is required"))?;
    let influx_bucket = env::var("INFLUX_BUCKET").unwrap_or_else(|_| "macrofactor".to_string());

    // --- MacroFactor auth ---
    // Prefer env var token, then home-directory config token, then email/password.
    let mut mf_client = if let Ok(token) = env::var("MACROFACTOR_REFRESH_TOKEN") {
        println!("Using MACROFACTOR_REFRESH_TOKEN from environment.");
        MacroFactorClient::new(token)
    } else if let Some(token) = read_refresh_token_from_config()? {
        println!("Using MACROFACTOR_REFRESH_TOKEN from ~/.macrofactor-influx/config.json.");
        MacroFactorClient::new(token)
    } else {
        let email = env::var("MACROFACTOR_EMAIL").map_err(|_| {
            anyhow!("Set MACROFACTOR_EMAIL, or provide MACROFACTOR_REFRESH_TOKEN via env/config")
        })?;
        let password = env::var("MACROFACTOR_PASSWORD").map_err(|_| {
            anyhow!("Set MACROFACTOR_PASSWORD, or provide MACROFACTOR_REFRESH_TOKEN via env/config")
        })?;

        println!("No refresh token found in env/config — signing in with email/password…");
        let refresh_token = firebase_sign_in(&email, &password).await?;
        write_refresh_token_to_config(&refresh_token)?;

        println!("Saved refresh token to ~/.macrofactor-influx/config.json.");

        MacroFactorClient::new(refresh_token)
    };

    // --- Date range ---
    let ingest_days: i64 = env::var("INGEST_DAYS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let update_days: i64 = env::var("UPDATE_DAYS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let end = Local::now().date_naive();
    let start = end - chrono::Duration::days(ingest_days);
    let update_start = end - chrono::Duration::days(update_days);

    // --- Fetch food log entries for each day ---
    // While fetching, track entry_ids that fall within the update window so we
    // can later identify stale entries in InfluxDB.
    println!("Fetching food log entries from {} to {}…", start, end);
    let mut food_points: Vec<DataPoint> = Vec::new();
    let mut fetched_ids_in_update_window: HashSet<String> = HashSet::new();
    let mut current = start;
    while current <= end {
        let entries = mf_client.get_food_log(current).await?;
        println!("  {}: {} food entries", current, entries.len());

        for entry in &entries {
            if entry.deleted == Some(true) {
                continue;
            }

            // Track IDs in the update window for staleness detection.
            if current >= update_start {
                fetched_ids_in_update_window.insert(entry.entry_id.clone());
            }

            let hour: u32 = entry
                .hour
                .as_deref()
                .and_then(|h| h.parse().ok())
                .unwrap_or(0);
            let minute: u32 = entry
                .minute
                .as_deref()
                .and_then(|m| m.parse().ok())
                .unwrap_or(0);
            let Some(naive_dt) = entry.date.and_hms_opt(hour, minute, 0) else {
                continue;
            };
            // Interpret hour/minute as local time, then convert to UTC.
            // Multiply by 1e9: write_with_precision below sends nanoseconds.
            let ts = Local
                .from_local_datetime(&naive_dt)
                .single()
                .map(|dt| dt.timestamp())
                .unwrap_or_else(|| naive_dt.and_utc().timestamp())
                * 1_000_000_000;

            let mut builder = DataPoint::builder("food_entry")
                .timestamp(ts)
                .tag("entry_id", &entry.entry_id);

            if let Some(name) = &entry.name {
                builder = builder.tag("name", name);
            }
            if let Some(brand) = &entry.brand {
                builder = builder.tag("brand", brand);
            }

            if let Some(v) = entry.calories() {
                builder = builder.field("calories", v);
            }
            if let Some(v) = entry.protein() {
                builder = builder.field("protein", v);
            }
            if let Some(v) = entry.carbs() {
                builder = builder.field("carbs", v);
            }
            if let Some(v) = entry.fat() {
                builder = builder.field("fat", v);
            }
            if let Some(v) = entry.weight_grams() {
                builder = builder.field("weight_grams", v);
            }

            match builder.build() {
                Ok(point) => food_points.push(point),
                Err(e) => eprintln!("Warning: skipping entry {}: {}", entry.entry_id, e),
            }
        }

        current += chrono::Duration::days(1);
    }

    let influx = InfluxClient::new(&influx_url, &influx_org, &influx_token);

    // --- Write food entries to InfluxDB ---
    // Write first so a subsequent delete failure never leaves the DB missing data
    // that still exists in MacroFactor.
    println!("Writing {} food entries to InfluxDB…", food_points.len());
    if !food_points.is_empty() {
        influx
            .write_with_precision(
                &influx_bucket,
                stream::iter(food_points),
                TimestampPrecision::Nanoseconds,
            )
            .await?;
    }

    // --- Delete stale food entries in the update window ---
    // Query InfluxDB for entry_ids that exist in the update window, then delete
    // any that are no longer present in the fresh fetch (i.e. deleted in MacroFactor).
    let update_start_str = format!("{}T00:00:00Z", update_start.format("%Y-%m-%d"));
    let update_stop_str = format!(
        "{}T00:00:00Z",
        (end + chrono::Duration::days(1)).format("%Y-%m-%d")
    );

    // Use a data query (not the schema API) so the time range is respected exactly.
    let flux = format!(
        r#"from(bucket: "{bucket}")
  |> range(start: {start}, stop: {stop})
  |> filter(fn: (r) => r._measurement == "food_entry")
  |> keep(columns: ["entry_id", "_time"])
  |> unique(column: "entry_id")"#,
        bucket = influx_bucket,
        start = update_start_str,
        stop = update_stop_str,
    );
    let existing_ids: HashSet<String> = influx
        .query_raw(Some(Query::new(flux)))
        .await?
        .into_iter()
        .filter_map(|r| match r.values.get("entry_id") {
            Some(InfluxValue::String(id)) => Some(id.clone()),
            _ => None,
        })
        .collect();

    let stale_ids: Vec<&String> = existing_ids
        .difference(&fetched_ids_in_update_window)
        .collect();

    if stale_ids.is_empty() {
        println!("No stale food entries in update window.");
    } else {
        println!("Deleting {} stale food entries…", stale_ids.len());
        // delete() takes NaiveDateTime; the crate serializes with a "Z" suffix,
        // so these must represent UTC midnight.
        let del_start = update_start.and_hms_opt(0, 0, 0).unwrap();
        let del_stop = (end + chrono::Duration::days(1))
            .and_hms_opt(0, 0, 0)
            .unwrap();
        for id in stale_ids {
            // Escape backslashes first, then double-quotes, before interpolating into the predicate.
            let safe_id = id.replace('\\', r#"\\"#).replace('"', r#"\""#);
            influx
                .delete(
                    &influx_bucket,
                    del_start,
                    del_stop,
                    Some(format!(
                        r#"_measurement="food_entry" AND entry_id="{}""#,
                        safe_id
                    )),
                )
                .await?;
        }
    }

    println!("Done.");
    Ok(())
}

/// Sign in to Firebase with email + password and return the refresh token.
/// This lets us print the token on first run so the user can persist it.
async fn firebase_sign_in(email: &str, password: &str) -> Result<String> {
    let url = format!(
        "https://identitytoolkit.googleapis.com/v1/accounts:signInWithPassword?key={}",
        FIREBASE_WEB_API_KEY
    );

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("X-Ios-Bundle-Identifier", "com.sbs.diet")
        .json(&serde_json::json!({
            "email": email,
            "password": password,
            "returnSecureToken": true
        }))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("Firebase sign-in failed: {} — {}", status, body));
    }

    let data: serde_json::Value = resp.json().await?;
    data["refreshToken"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| anyhow!("No refreshToken field in Firebase sign-in response"))
}

fn config_path() -> Result<PathBuf> {
    let home = env::var("HOME").map_err(|_| anyhow!("HOME env var is not set"))?;
    Ok(PathBuf::from(home)
        .join(".macrofactor-influx")
        .join("config.json"))
}

fn read_refresh_token_from_config() -> Result<Option<String>> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(None);
    }

    let raw = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) => {
            eprintln!("Warning: could not read {}: {}", path.display(), err);
            return Ok(None);
        }
    };

    let parsed: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("Warning: invalid JSON in {}: {}", path.display(), err);
            return Ok(None);
        }
    };
    let token = parsed
        .get("MACROFACTOR_REFRESH_TOKEN")
        .and_then(|v| v.as_str())
        .map(String::from);

    Ok(token)
}

fn write_refresh_token_to_config(token: &str) -> Result<()> {
    let path = config_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("Invalid config path: {}", path.display()))?;
    fs::create_dir_all(parent)?;

    // Preserve existing keys while updating the refresh token.
    let mut config = if path.exists() {
        match fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        {
            Some(serde_json::Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        }
    } else {
        serde_json::Map::new()
    };

    config.insert(
        "MACROFACTOR_REFRESH_TOKEN".to_string(),
        serde_json::Value::String(token.to_string()),
    );

    let content = serde_json::to_string_pretty(&serde_json::Value::Object(config))?;
    fs::write(path, format!("{}\n", content))?;
    Ok(())
}
