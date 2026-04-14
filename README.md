# macrofactor-influx

Pulls nutrition data from MacroFactor and writes it to InfluxDB v2 on a daily schedule.

## What it does

This tool fetches food log entries from
[MacroFactor](https://www.macrofactorapp.com/) using the unofficial
[macro-factor-api](https://crates.io/crates/macro-factor-api) Rust crate, which reads
directly from MacroFactor's Firestore backend.

Data is written to InfluxDB v2 as the `food_entry` measurement. It is designed to run
on a schedule inside Docker via [supercronic](https://github.com/aptible/supercronic).

## Prerequisites

- Docker
- A MacroFactor account **with a password set** (required for Firebase authentication —
  Sign in with Apple / Google alone won't work)

## Setup

```sh
git clone https://github.com/aromberg/macrofactor-influx
cd macrofactor-influx
cp .env.example .env
# Fill in credentials (see Configuration below)
docker run --rm --env-file .env macrofactor-influx
```

## Configuration

| Variable | Default | Description |
|---|---|---|
| `MACROFACTOR_EMAIL` | — | MacroFactor account email |
| `MACROFACTOR_PASSWORD` | — | MacroFactor account password |
| `MACROFACTOR_REFRESH_TOKEN` | — | Firebase refresh token (highest priority; overrides config file) |
| `INFLUX_URL` | — | InfluxDB base URL |
| `INFLUX_ORG` | — | InfluxDB organisation name |
| `INFLUX_TOKEN` | — | InfluxDB all-access token |
| `INFLUX_BUCKET` | `macrofactor` | InfluxDB bucket to write into |
| `INGEST_DAYS` | `2` | How many days back to fetch and write |
| `UPDATE_DAYS` | `1` | How many recent days to check for deleted entries |

## Authentication

At runtime, MacroFactor auth is resolved in this order:

1. `MACROFACTOR_REFRESH_TOKEN` environment variable
2. `~/.macrofactor-influx/config.json` (`MACROFACTOR_REFRESH_TOKEN` key)
3. Email/password login (`MACROFACTOR_EMAIL` + `MACROFACTOR_PASSWORD`)

If email/password login is used, the new refresh token is automatically written to:

```
~/.macrofactor-influx/config.json
```

After first successful login, subsequent runs can use that saved token without
requiring password auth.

## Schedule

The ingester runs **every hour** as defined in `crontab`:

```
0 * * * * /usr/local/bin/app
```

To change the schedule, edit `crontab` and rebuild the image.