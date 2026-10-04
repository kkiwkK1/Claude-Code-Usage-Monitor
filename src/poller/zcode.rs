//! ZCode Coding Plan usage, read from the ZCode CLI's local database.
//!
//! The ZCode CLI records every model request in
//! `%USERPROFILE%\.zcode\cli\db\db.sqlite` (table `model_usage`, timestamps in
//! Unix milliseconds). Coding Plan traffic is identifiable by its
//! `provider_id`, which ends in `coding-plan` — for example
//! `builtin:bigmodel-coding-plan` or
//! `account:bigmodel-individual-coding-plan`. Other plans the CLI can use
//! (start plan, off-peak idle) are deliberately excluded.
//!
//! No local source publishes the plan's quota, so the gauge denominators are
//! opt-in environment variables holding the limits printed on the plan page:
//!
//! - `ZCODE_FIVE_HOUR_QUOTA` — prompts allowed per rolling 5-hour window
//! - `ZCODE_WEEKLY_QUOTA`    — prompts allowed per rolling 7-day window
//!
//! Without them the windows stay available with a 0 % gauge, and the raw
//! prompt/token counts are still exposed through `zcode.limits.*` so themes
//! can display absolutes.
//!
//! The database is only ever opened read-only through Windows' bundled
//! SQLite, so polling can never disturb a running CLI.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::PollError;
use crate::diagnose;
use crate::models::{UsageData, UsageLimit, UsageSection};

const DB_OVERRIDE_ENV: &str = "ZCODE_DB_PATH";
const FIVE_HOUR_QUOTA_ENV: &str = "ZCODE_FIVE_HOUR_QUOTA";
const WEEKLY_QUOTA_ENV: &str = "ZCODE_WEEKLY_QUOTA";

/// Coding Plan rows only; start/off-peak plans are a different allowance.
const PROVIDER_FILTER: &str = "%coding-plan";

const FIVE_HOURS_MS: i64 = 5 * 60 * 60 * 1000;
const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;

struct WindowTotals {
    prompts: u64,
    tokens: i64,
    oldest_started_at_ms: i64,
}

fn database_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(DB_OVERRIDE_ENV) {
        return Some(PathBuf::from(path));
    }
    std::env::var_os("USERPROFILE").map(|profile| {
        PathBuf::from(profile).join(r".zcode\cli\db\db.sqlite")
    })
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

/// Aggregates one rolling window with a single bound parameter (the provider
/// filter); the window bound is a compiled-in integer so it is formatted
/// directly instead of bound.
fn window_totals(since_ms: i64) -> Result<WindowTotals, PollError> {
    let path = database_path().ok_or(PollError::NoCredentials)?;
    if !path.is_file() {
        return Err(PollError::NoCredentials);
    }

    let sql = format!(
        "SELECT CAST(IFNULL(SUM(computed_total_tokens), 0) AS TEXT) || ' ' || \
         CAST(COUNT(DISTINCT logical_request_id) AS TEXT) || ' ' || \
         CAST(IFNULL(MIN(started_at), 0) AS TEXT) \
         FROM model_usage WHERE provider_id LIKE ?1 AND started_at >= {since_ms}"
    );
    let row = crate::winsqlite::query_optional_text(&path, &sql, PROVIDER_FILTER)
        .map_err(|error| {
            diagnose::log(format!("zcode query failed: {error}"));
            PollError::UnexpectedResponse
        })?
        .unwrap_or_default();

    // The aggregate is a single "tokens prompts oldest" triple.
    let mut fields = row.split_whitespace();
    let tokens = fields.next().and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    let prompts = fields
        .next()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0) as u64;
    let oldest_started_at_ms = fields
        .next()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);

    Ok(WindowTotals {
        prompts,
        tokens,
        oldest_started_at_ms,
    })
}

fn quota_from_env(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.trim().parse::<u64>().ok().filter(|v| *v > 0)
}

fn window_section(
    totals: &WindowTotals,
    quota: Option<u64>,
    window_ms: i64,
) -> UsageSection {
    let percentage = quota
        .map(|quota| (totals.prompts as f64 / quota as f64) * 100.0)
        .unwrap_or(0.0)
        .clamp(0.0, 100.0);
    // A rolling window drains completely once its oldest request ages out.
    let resets_at = (totals.oldest_started_at_ms > 0).then(|| {
        UNIX_EPOCH + Duration::from_millis((totals.oldest_started_at_ms + window_ms) as u64)
    });
    UsageSection {
        available: true,
        percentage,
        resets_at,
    }
}

fn token_limit(
    key: &str,
    label: &str,
    totals: &WindowTotals,
    window_ms: i64,
) -> UsageLimit {
    UsageLimit {
        key: key.into(),
        kind: "tokens".into(),
        label: label.into(),
        model: None,
        model_id: None,
        // Absolute counts for themes; the headline gauge needs a quota.
        scope: Some(serde_json::json!({
            "prompts": totals.prompts,
            "tokens": totals.tokens,
        })),
        is_active: totals.prompts > 0,
        usage: UsageSection {
            available: totals.prompts > 0,
            percentage: 0.0,
            resets_at: (totals.oldest_started_at_ms > 0).then(|| {
                UNIX_EPOCH
                    + Duration::from_millis((totals.oldest_started_at_ms + window_ms) as u64)
            }),
        },
    }
}

pub(super) fn poll_zcode() -> Result<UsageData, PollError> {
    let now = now_ms();
    let five_hour = window_totals(now - FIVE_HOURS_MS)?;
    let weekly = window_totals(now - WEEK_MS)?;

    let five_hour_quota = quota_from_env(FIVE_HOUR_QUOTA_ENV);
    let weekly_quota = quota_from_env(WEEKLY_QUOTA_ENV);
    if five_hour_quota.is_none() || weekly_quota.is_none() {
        diagnose::log(format!(
            "zcode quotas not configured: {FIVE_HOUR_QUOTA_ENV}={} {WEEKLY_QUOTA_ENV}={} \
             (gauge stays at 0% until they are set)",
            five_hour_quota.is_some(),
            weekly_quota.is_some(),
        ));
    }

    Ok(UsageData {
        session: window_section(&five_hour, five_hour_quota, FIVE_HOURS_MS),
        weekly: window_section(&weekly, weekly_quota, WEEK_MS),
        weekly_label: Some("7d".into()),
        monthly: None,
        credits: None,
        limits: vec![
            token_limit("five_hour", "5h tokens", &five_hour, FIVE_HOURS_MS),
            token_limit("weekly", "7d tokens", &weekly, WEEK_MS),
        ],
        stale: false,
    })
}

/// The "credential" for this provider is the CLI's database file: watching it
/// is what lets a freshly installed CLI be picked up.
pub(super) fn credential_watch_snapshot(_all_sources: bool) -> Vec<String> {
    let Some(path) = database_path() else {
        return vec!["zcode:db-path-missing".into()];
    };
    vec![path_signature("zcode", &path)]
}

fn path_signature(kind: &str, path: &Path) -> String {
    let key = format!("{kind}:{}", path.display());
    match std::fs::metadata(path) {
        Ok(metadata) => {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .map(|value| value.as_nanos())
                .unwrap_or(0);
            format!("{key}|present|{}|{modified}", metadata.len())
        }
        Err(_) => format!("{key}|missing"),
    }
}
