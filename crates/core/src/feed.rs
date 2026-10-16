//! The release feed format (ArtCraft's `release_info` shape), read only from a developer's own
//! local mirror (`cargo xtask local serve`, behind the `local-mirror` feature that release builds
//! never enable). The launcher never fetches ArtCraft's feed: releases come from GitHub, at the
//! repos the catalog names.
//!
//! The feed is input like any other: every field is optional, unknown fields are ignored, a field
//! of the wrong type reads as missing, one bad entry never spoils the others, and download links
//! must point at the app's own GitHub releases or sit under the mirror (see [`Feed::with_mirror`]).
//! A feed can't move an app to another repo.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::de::{DeserializeOwned, Deserializer};

use crate::catalog::CraftApp;
use crate::release::{self, Asset, Release};
use crate::time::parse_rfc3339;
use crate::version;

/// An app's entry is trusted for this long after it was fetched (a mirror's never goes stale).
pub const MAX_AGE: i64 = 24 * 3600;

/// The feed, read leniently.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Feed {
    /// By lowercase app name.
    pub apps: BTreeMap<String, FeedApp>,
    /// When the least recently fetched app was fetched (Unix seconds).
    pub last_fetched: Option<i64>,
    pub announcements: Vec<Notice>,
    pub alerts: Vec<Notice>,
    /// A local mirror's address (ending in `/`): downloads under it are accepted alongside GitHub's.
    /// Never read from the feed itself; the caller sets it with [`Feed::with_mirror`].
    pub mirror: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FeedApp {
    pub repo: Option<String>,
    /// Present when the repo has a published release. Meaningless unless `fetched_at` is set.
    pub release: Option<FeedRelease>,
    /// When this app was last fetched (Unix seconds).
    pub fetched_at: Option<i64>,
}

/// A release as the feed describes it, before it's checked against the app's repo.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FeedRelease {
    pub tag: String,
    pub name: String,
    pub published_at: Option<i64>,
    pub html_url: String,
    pub prerelease: bool,
    pub notes: String,
    pub assets: Vec<(String, u64, String, Option<String>)>,
}

/// An announcement (shown inline, dismissible) or an alert (a popup the user acknowledges).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Notice {
    /// Stable, so a dismissal sticks across launches and refreshes.
    pub id: String,
    pub title: String,
    pub body: String,
    /// Only `https://` links survive parsing.
    pub link: Option<String>,
    pub created_at: Option<i64>,
}

/// What the feed says about one app, once checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeedAnswer {
    /// Fresh: the app's latest release, or `None` when it has none.
    Fresh(Option<Release>),
    /// Not listed, never fetched, or older than [`MAX_AGE`]: ask GitHub.
    AskGitHub,
}

const MAX_NOTICES: usize = 20;
const MAX_TEXT: usize = 4000;

impl Feed {
    /// Reads the feed. `None` only when the answer isn't JSON (or isn't an object) at all.
    pub fn parse(json: &str) -> Option<Self> {
        let raw: RawResponse = serde_json::from_str(json).ok()?;
        Some(Feed {
            apps: raw.release_info.apps.into_iter().map(|(k, v)| (k.to_lowercase(), v.into_app())).collect(),
            last_fetched: raw.release_info.last_fetched.as_deref().and_then(parse_rfc3339),
            announcements: raw.important_notices.announcements.into_iter().filter_map(|n| n.into_notice("announcement")).take(MAX_NOTICES).collect(),
            alerts: raw.important_notices.alerts.into_iter().filter_map(|n| n.into_notice("alert")).take(MAX_NOTICES).collect(),
            mirror: None,
        })
    }

    /// Trusts downloads under `base` (which must end in `/`) and keeps every entry fresh: a feed
    /// served by `cargo xtask local serve` on the developer's own machine.
    #[must_use]
    pub fn with_mirror(mut self, base: &str) -> Self {
        self.mirror = base.ends_with('/').then(|| base.to_string());
        self
    }

    /// What the feed says about `app` at `now`.
    pub fn answer(&self, app: &CraftApp, now: i64) -> FeedAnswer {
        let Some(entry) = self.apps.get(app.id) else { return FeedAnswer::AskGitHub };
        let Some(fetched_at) = entry.fetched_at else { return FeedAnswer::AskGitHub };
        // A timestamp from the future (clock skew) counts as just now; one too old doesn't count.
        // A local mirror is as fresh as its last sync.
        if self.mirror.is_none() && now.saturating_sub(fetched_at) > MAX_AGE {
            return FeedAnswer::AskGitHub;
        }
        // The feed's repo, when it names one, must be the app's own.
        let repo = app.repo;
        if let Some(feed_repo) = &entry.repo
            && !feed_repo.eq_ignore_ascii_case(repo)
        {
            return FeedAnswer::AskGitHub;
        }
        match &entry.release {
            None => FeedAnswer::Fresh(None),
            Some(r) => match r.to_release(repo, self.mirror.as_deref()) {
                Some(release) => FeedAnswer::Fresh(Some(release)),
                None => FeedAnswer::AskGitHub,
            },
        }
    }
}

impl FeedRelease {
    /// The release with only the downloads that belong to `repo` (or sit under `mirror`). `None`
    /// when it doesn't hold up (no tag, or not a version).
    fn to_release(&self, repo: &str, mirror: Option<&str>) -> Option<Release> {
        let version = version::parse(&self.tag)?;
        let prefix = release::download_prefix(repo);
        let allowed = |url: &str| url.starts_with(&prefix) || mirror.is_some_and(|m| url.starts_with(m) && !url.contains(".."));
        let releases_page = format!("https://github.com/{repo}/releases/");
        let assets = self
            .assets
            .iter()
            .filter(|(name, _, url, _)| release::valid_file_name(name) && allowed(url) && !url.contains(['?', '#', '\\']))
            .map(|(name, size, url, sha)| Asset {
                name: name.clone(),
                size: *size,
                url: url.clone(),
                sha256: sha.clone().filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())).map(|h| h.to_ascii_lowercase()),
            })
            .collect();
        Some(Release {
            version: Some(version),
            name: if self.name.is_empty() { self.tag.clone() } else { truncate(&self.name, 120) },
            tag: self.tag.clone(),
            published_at: self.published_at,
            html_url: if self.html_url.starts_with(&releases_page) { self.html_url.clone() } else { format!("{releases_page}tag/{}", self.tag) },
            notes: truncate(&self.notes, 64 * 1024),
            prerelease: self.prerelease,
            assets,
        })
    }
}

// ---- lenient wire format ----------------------------------------------------------------------

/// Reads `T`, or its default when the value is missing, `null` or the wrong shape.
fn lenient<'de, D: Deserializer<'de>, T: DeserializeOwned + Default>(d: D) -> Result<T, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(T::deserialize(value).unwrap_or_default())
}

/// A list where entries of the wrong shape are dropped rather than spoiling the rest.
fn lenient_vec<'de, D: Deserializer<'de>, T: DeserializeOwned>(d: D) -> Result<Vec<T>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    let items = match value {
        serde_json::Value::Array(items) => items,
        _ => return Ok(Vec::new()),
    };
    Ok(items.into_iter().filter_map(|v| T::deserialize(v).ok()).collect())
}

/// A map where entries of the wrong shape are dropped.
fn lenient_map<'de, D: Deserializer<'de>, T: DeserializeOwned>(d: D) -> Result<BTreeMap<String, T>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    let entries = match value {
        serde_json::Value::Object(entries) => entries,
        _ => return Ok(BTreeMap::new()),
    };
    Ok(entries.into_iter().filter_map(|(k, v)| T::deserialize(v).ok().map(|t| (k, t))).collect())
}

#[derive(Deserialize, Default)]
struct RawResponse {
    #[serde(default, deserialize_with = "lenient")]
    release_info: RawReleaseInfo,
    #[serde(default, deserialize_with = "lenient")]
    important_notices: RawNotices,
}

#[derive(Deserialize, Default)]
struct RawReleaseInfo {
    #[serde(default, deserialize_with = "lenient_map")]
    apps: BTreeMap<String, RawApp>,
    #[serde(default, deserialize_with = "lenient")]
    last_fetched: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawApp {
    #[serde(default, deserialize_with = "lenient")]
    repo: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    latest_release: Option<RawRelease>,
    #[serde(default, deserialize_with = "lenient")]
    app_last_fetched: Option<String>,
}

impl RawApp {
    fn into_app(self) -> FeedApp {
        FeedApp {
            repo: self.repo.filter(|r| r.split('/').count() == 2),
            release: self.latest_release.and_then(RawRelease::into_release),
            fetched_at: self.app_last_fetched.as_deref().and_then(parse_rfc3339),
        }
    }
}

#[derive(Deserialize, Default)]
struct RawRelease {
    #[serde(default, deserialize_with = "lenient")]
    tag: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    name: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    published_at: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    html_url: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    prerelease: bool,
    #[serde(default, deserialize_with = "lenient")]
    notes_markdown: Option<String>,
    #[serde(default, deserialize_with = "lenient_vec")]
    assets: Vec<RawAsset>,
}

impl RawRelease {
    fn into_release(self) -> Option<FeedRelease> {
        let tag = self.tag.filter(|t| !t.is_empty() && t.len() <= 64)?;
        Some(FeedRelease {
            tag,
            name: self.name.unwrap_or_default(),
            published_at: self.published_at.as_deref().and_then(parse_rfc3339),
            html_url: self.html_url.unwrap_or_default(),
            prerelease: self.prerelease,
            notes: self.notes_markdown.unwrap_or_default(),
            assets: self.assets.into_iter().filter_map(|a| Some((a.name?, a.size_bytes, a.download_url?, a.sha256))).take(300).collect(),
        })
    }
}

#[derive(Deserialize, Default)]
struct RawAsset {
    #[serde(default, deserialize_with = "lenient")]
    name: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    size_bytes: u64,
    #[serde(default, deserialize_with = "lenient")]
    download_url: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    sha256: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawNotices {
    #[serde(default, deserialize_with = "lenient_vec")]
    announcements: Vec<RawNotice>,
    #[serde(default, deserialize_with = "lenient_vec")]
    alerts: Vec<RawNotice>,
}

#[derive(Deserialize, Default)]
struct RawNotice {
    #[serde(default, deserialize_with = "lenient")]
    title: String,
    #[serde(default, deserialize_with = "lenient")]
    body: String,
    #[serde(default, deserialize_with = "lenient")]
    maybe_web_link: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    created_at: Option<String>,
}

impl RawNotice {
    fn into_notice(self, kind: &str) -> Option<Notice> {
        if self.title.trim().is_empty() && self.body.trim().is_empty() {
            return None;
        }
        let link = self.maybe_web_link.filter(|l| l.starts_with("https://") && l.len() <= 2000 && !l.chars().any(char::is_whitespace));
        Some(Notice {
            id: format!("{kind}:{}:{}", self.created_at.as_deref().unwrap_or(""), self.title.trim()),
            title: truncate(self.title.trim(), 200),
            body: truncate(self.body.trim(), MAX_TEXT),
            link,
            created_at: self.created_at.as_deref().and_then(parse_rfc3339),
        })
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{CINCH_OWNER, CraftApp, LAUNCHER};

    const NOW: i64 = 1_791_500_000; // 2026-10-09T…
    const HASH: &str = "8a5edab282632443219e051e4ade2d1d5bbc671c781051bf1437897cbdfea0f1";

    /// A feed in the `release_info` shape, abridged.
    fn sample() -> String {
        format!(
            r#"{{
        "success": true,
        "release_info": {{
          "apps": {{
            "photocraft": {{
              "name": "PhotoCraft", "repo": "{CINCH_OWNER}/photocraft",
              "latest_release": {{
                "tag": "v0.6.0", "version": "0.6.0", "name": "PhotoCraft v0.6.0",
                "published_at": "2026-10-09T10:00:00Z",
                "html_url": "https://github.com/{CINCH_OWNER}/photocraft/releases/tag/v0.6.0",
                "prerelease": false, "notes_markdown": "Notes",
                "assets": [
                  {{ "name": "photocraft-0.6.0-macos-universal.dmg", "size_bytes": 123,
                     "download_url": "https://github.com/{CINCH_OWNER}/photocraft/releases/download/v0.6.0/photocraft-0.6.0-macos-universal.dmg",
                     "sha256": "{HASH}", "platform": {{ "os": "macos", "arch": "universal", "package_type": "dmg" }} }},
                  {{ "name": "evil.dmg", "size_bytes": 1, "download_url": "https://evil.example/evil.dmg", "sha256": null }}
                ]
              }},
              "app_last_fetched": "2026-10-09T13:00:00Z"
            }},
            "soundcraft": {{ "name": "SoundCraft", "repo": "{CINCH_OWNER}/soundcraft", "latest_release": null,
                             "app_last_fetched": "2026-10-09T13:00:00Z" }},
            "filmcraft": {{ "name": "FilmCraft", "repo": "{CINCH_OWNER}/filmcraft", "latest_release": null, "app_last_fetched": null }},
            "vectorcraft": {{ "repo": "{CINCH_OWNER}/vectorcraft", "app_last_fetched": "2026-10-01T00:00:00Z",
                              "latest_release": {{ "tag": "v0.7.0" }} }},
            "brandnewcraft": {{ "repo": "{CINCH_OWNER}/brandnewcraft", "app_last_fetched": "2026-10-09T13:00:00Z" }}
          }},
          "last_fetched": "2026-10-09T13:00:00Z",
          "some_future_field": {{ "whatever": [1, 2, 3] }}
        }},
        "important_notices": {{ "announcements": [], "alerts": [] }}
      }}"#
        )
    }

    fn app(id: &str) -> &'static CraftApp {
        CraftApp::by_id(id).unwrap()
    }

    #[test]
    fn uses_fresh_entries_and_falls_back_for_everything_else() {
        let feed = Feed::parse(&sample()).unwrap();
        // Fresh, with a release: only the download that belongs to the repo survives.
        let FeedAnswer::Fresh(Some(r)) = feed.answer(app("photocraft"), NOW) else { panic!() };
        assert_eq!(r.version_label(), "0.6.0");
        assert_eq!(r.assets.len(), 1);
        assert_eq!(r.assets[0].sha256.as_deref(), Some(HASH));
        assert_eq!(r.published_at, parse_rfc3339("2026-10-09T10:00:00Z"));
        // Fresh, and no release yet.
        assert_eq!(feed.answer(app("soundcraft"), NOW), FeedAnswer::Fresh(None));
        // Never fetched, more than a day old, or not listed: ask GitHub.
        assert_eq!(feed.answer(app("filmcraft"), NOW), FeedAnswer::AskGitHub);
        assert_eq!(feed.answer(app("vectorcraft"), NOW), FeedAnswer::AskGitHub);
        assert_eq!(feed.answer(app("wordcraft"), NOW), FeedAnswer::AskGitHub);
        assert_eq!(feed.answer(&LAUNCHER, NOW), FeedAnswer::AskGitHub);
        // An app the launcher doesn't know is simply carried along.
        assert!(feed.apps.contains_key("brandnewcraft"));
    }

    #[test]
    fn survives_anything_the_server_might_send() {
        for junk in ["", "null", "[]", "42", "<html>502 Bad Gateway</html>", "{\"success\":tru"] {
            let feed = Feed::parse(junk);
            if let Some(feed) = feed {
                assert!(feed.apps.is_empty(), "{junk}");
            }
        }
        // Every field the wrong type: nothing panics, nothing is trusted.
        let weird = r#"{"release_info": {"apps": {"photocraft": {"repo": 7, "latest_release": "yes",
        "app_last_fetched": ["no"]}, "pdfcraft": 5}, "last_fetched": {}},
        "important_notices": {"announcements": [null, {"title": 3}], "alerts": "x"}}"#;
        let feed = Feed::parse(weird).unwrap();
        assert_eq!(feed.answer(app("photocraft"), NOW), FeedAnswer::AskGitHub);
        assert!(feed.announcements.is_empty() && feed.alerts.is_empty());
        // An empty object is a feed that knows nothing.
        assert_eq!(Feed::parse("{}").unwrap(), Feed::default());
    }

    #[test]
    fn never_trusts_a_repo_other_than_the_apps_own() {
        // Repo renames are no longer read: a feed naming another repo is simply not trusted.
        let moved = sample()
            .replace(&format!("\"repo\": \"{CINCH_OWNER}/photocraft\""), "\"repo\": \"art/photocraft\"")
            .replace(&format!("github.com/{CINCH_OWNER}/photocraft/releases"), "github.com/art/photocraft/releases")
            .replace(
                "\"important_notices\"",
                &format!(r#""repo_renames": [{{"old_name": "{CINCH_OWNER}/photocraft", "new_name": "art/photocraft"}}], "important_notices""#),
            );
        let feed = Feed::parse(&moved).unwrap();
        assert_eq!(feed.answer(app("photocraft"), NOW), FeedAnswer::AskGitHub);
        // Without a repo of its own, an entry's downloads must still sit under the app's repo.
        let unnamed = moved.replace("\"repo\": \"art/photocraft\",", "");
        let FeedAnswer::Fresh(Some(r)) = Feed::parse(&unnamed).unwrap().answer(app("photocraft"), NOW) else { panic!() };
        assert!(r.assets.is_empty());
    }

    #[test]
    fn a_local_mirror_is_trusted_only_when_asked_for_and_never_goes_stale() {
        const MIRROR: &str = "http://127.0.0.1:8787/";
        let local = sample()
            .replace(&format!("https://github.com/{CINCH_OWNER}/photocraft/releases/download/v0.6.0/"), "http://127.0.0.1:8787/photocraft/v0.6.0/");
        // Without the mirror, the local link is dropped like any other stranger's.
        let FeedAnswer::Fresh(Some(r)) = Feed::parse(&local).unwrap().answer(app("photocraft"), NOW) else { panic!() };
        assert!(r.assets.is_empty());
        let feed = Feed::parse(&local).unwrap().with_mirror(MIRROR);
        let FeedAnswer::Fresh(Some(r)) = feed.answer(app("photocraft"), NOW) else { panic!() };
        assert_eq!(r.assets.len(), 1, "the mirror's file is kept, evil.example's still isn't");
        assert_eq!(r.assets[0].url, "http://127.0.0.1:8787/photocraft/v0.6.0/photocraft-0.6.0-macos-universal.dmg");
        // A week-old sync still counts.
        assert!(matches!(feed.answer(app("photocraft"), NOW + 7 * 86_400), FeedAnswer::Fresh(Some(_))));
        // Escaping the mirror's folder isn't allowed, and a base without its trailing slash isn't a mirror.
        let escaping = local.replace("http://127.0.0.1:8787/photocraft/", "http://127.0.0.1:8787/../photocraft/");
        let FeedAnswer::Fresh(Some(r)) = Feed::parse(&escaping).unwrap().with_mirror(MIRROR).answer(app("photocraft"), NOW) else { panic!() };
        assert!(r.assets.is_empty());
        assert_eq!(Feed::default().with_mirror("http://127.0.0.1:8787").mirror, None);
    }

    #[test]
    fn reads_notices_with_https_links_only() {
        let with_notices = sample()
            .replace(
                "\"announcements\": []",
                r#""announcements": [{"title": "New: CADCraft 1.0", "body": "It's here.", "created_at": "2026-10-09T09:00:00Z"}]"#,
            )
            .replace(
                "\"alerts\": []",
                r#""alerts": [
            {"title": "Security update", "body": "Update PdfCraft now.", "maybe_web_link": "https://example.com/security", "created_at": null},
            {"title": "Plain link", "body": "x", "maybe_web_link": "http://example.com/security"},
            {"title": "Bad link", "body": "x", "maybe_web_link": "javascript:alert(1)"}]"#,
            )
            .replace(
                "\"important_notices\"",
                r#""app_renames": [{"old_name": "PdfCraft", "old_name_lower": "pdfcraft", "new_name": "DocuCraft",
            "new_name_lower": "docucraft", "description": "Same app, new name.", "renamed_at": "2026-10-09T08:00:00Z"}], "important_notices""#,
            );
        let feed = Feed::parse(&with_notices).unwrap();
        assert_eq!(feed.alerts.len(), 3);
        assert_eq!(feed.alerts[0].link.as_deref(), Some("https://example.com/security"));
        assert_eq!(feed.alerts[1].link, None, "plain http links are dropped");
        assert_eq!(feed.alerts[2].link, None, "only web links are kept");
        // App renames are no longer read, so they don't turn into announcements either.
        let titles: Vec<&str> = feed.announcements.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(titles, ["New: CADCraft 1.0"]);
        // Ids are stable across parses, so dismissals stick.
        assert_eq!(Feed::parse(&with_notices).unwrap().alerts[0].id, feed.alerts[0].id);
    }
}
