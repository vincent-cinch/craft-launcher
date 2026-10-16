//! The Craft App Suite: the twelve apps the launcher knows about.
//!
//! The list is data so a later version can fetch it from an API; [`SUITE`] is the built-in copy.
//!
//! Text here is English, the source the UI translates from: each `tagline` has a catalog row
//! keyed by the app id (`tr_ctx(app.id, app.tagline)` in `ui-egui`).

use serde::{Deserialize, Serialize};

/// The GitHub owner of every Craft App fork and of the launcher itself. The one place the owner
/// is named: change it here to move every repo at once. A macro rather than a `const` so
/// `concat!` can build the repo names below from it.
macro_rules! cinch_owner {
    () => {
        "vincent-cinch"
    };
}

/// `<owner>/<name>` under [`CINCH_OWNER`].
macro_rules! cinch_repo {
    ($name:literal) => {
        concat!(cinch_owner!(), "/", $name)
    };
}

/// The GitHub owner every app and the launcher are installed from.
pub const CINCH_OWNER: &str = cinch_owner!();

/// Which generation of the suite an app belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AppGroup {
    /// The seven original apps, each with a page on getartcraft.com.
    Classic,
    /// The five apps added after them.
    New,
}

impl AppGroup {
    pub fn label(self) -> &'static str {
        match self {
            Self::Classic => "Classic",
            Self::New => "New",
        }
    }
}

/// One app in the suite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CraftApp {
    /// Machine name: repo, binary, package and asset stem (`photocraft`).
    pub id: &'static str,
    /// Display name (`PhotoCraft`).
    pub name: &'static str,
    /// What it's for, in a few words (English; the UI translates it by app id).
    pub tagline: &'static str,
    /// The familiar app it reimplements, for orientation.
    pub like: &'static str,
    pub group: AppGroup,
    /// Brand colour (sRGB), for surfaces: getartcraft.com's `--app`.
    pub color: [u8; 3],
    /// The brand colour for text on a light background (the site's light `--app-ink`).
    pub ink_light: [u8; 3],
    /// The brand colour for text on a dark background (the site's dark `--app-ink`).
    pub ink_dark: [u8; 3],
    /// GitHub `owner/repo`.
    pub repo: &'static str,
    /// Older names its release assets may carry (PdfCraft shipped as PrintCraft until 0.2.1).
    pub former_ids: &'static [&'static str],
}

impl CraftApp {
    /// The product it's inspired by, with its registered-trademark sign (`Adobe Photoshop®`).
    pub fn inspired_by(&self) -> String {
        format!("{}®", self.like)
    }

    /// `ai.storyteller.<id>`: macOS bundle id, Flatpak id and Linux desktop-file id.
    pub fn bundle_id(&self) -> String {
        format!("ai.storyteller.{}", self.id)
    }

    pub fn releases_url(&self) -> String {
        format!("https://github.com/{}/releases", self.repo)
    }

    pub fn repo_url(&self) -> String {
        format!("https://github.com/{}", self.repo)
    }

    /// The app's page on getartcraft.com (classic apps only).
    pub fn website(&self) -> Option<String> {
        (self.group == AppGroup::Classic).then(|| format!("https://getartcraft.com/apps/{}", self.id))
    }

    /// Asset stems this app's releases may use, current first.
    pub fn stems(&self) -> impl Iterator<Item = &'static str> + '_ {
        std::iter::once(self.id).chain(self.former_ids.iter().copied())
    }

    pub fn by_id(id: &str) -> Option<&'static CraftApp> {
        SUITE.iter().find(|a| a.id == id)
    }
}

const fn app(
    id: &'static str,
    name: &'static str,
    tagline: &'static str,
    like: &'static str,
    group: AppGroup,
    [color, ink_light, ink_dark]: [[u8; 3]; 3],
    repo: &'static str,
) -> CraftApp {
    CraftApp { id, name, tagline, like, group, color, ink_light, ink_dark, repo, former_ids: &[] }
}

/// The Craft App Suite, in display order: the seven classic apps, then the five new ones.
pub const SUITE: [CraftApp; 12] = [
    app(
        "photocraft",
        "PhotoCraft",
        "Image editing",
        "Adobe Photoshop",
        AppGroup::Classic,
        [[0x2f, 0x7b, 0xf5], [0x1a, 0x5b, 0xc4], [0x7f, 0xb0, 0xff]],
        cinch_repo!("photocraft"),
    ),
    app(
        "vectorcraft",
        "VectorCraft",
        "Vector illustration",
        "Adobe Illustrator",
        AppGroup::Classic,
        [[0xe8, 0x57, 0x3f], [0xb8, 0x3a, 0x24], [0xff, 0x8a, 0x70]],
        cinch_repo!("vectorcraft"),
    ),
    app(
        "filmcraft",
        "FilmCraft",
        "Video editing, color and sound",
        "Adobe Premiere Pro",
        AppGroup::Classic,
        [[0x8b, 0x5c, 0xf6], [0x6a, 0x3f, 0xd6], [0xb6, 0x9c, 0xff]],
        cinch_repo!("filmcraft"),
    ),
    app(
        "lightcraft",
        "LightCraft",
        "Photo library and raw development",
        "Adobe Lightroom",
        AppGroup::Classic,
        [[0xf2, 0xa5, 0x16], [0x8a, 0x58, 0x00], [0xff, 0xc1, 0x4d]],
        cinch_repo!("lightcraft"),
    ),
    CraftApp {
        former_ids: &["printcraft"],
        ..app(
            "pdfcraft",
            "PdfCraft",
            "Reading, organizing and protecting PDFs",
            "Adobe Acrobat",
            AppGroup::Classic,
            [[0x12, 0xa5, 0x8a], [0x0a, 0x75, 0x63], [0x3f, 0xd6, 0xb8]],
            cinch_repo!("pdfcraft"),
        )
    },
    app(
        "effectcraft",
        "EffectCraft",
        "Motion graphics and visual effects",
        "Adobe After Effects",
        AppGroup::Classic,
        [[0xe0, 0x36, 0x8f], [0xb0, 0x20, 0x6c], [0xff, 0x7a, 0xb8]],
        cinch_repo!("effectcraft"),
    ),
    app(
        "designcraft",
        "DesignCraft",
        "Page layout and publishing",
        "Adobe InDesign",
        AppGroup::Classic,
        [[0x7b, 0xb5, 0x1c], [0x45, 0x70, 0x08], [0xa6, 0xe0, 0x4a]],
        cinch_repo!("designcraft"),
    ),
    app(
        "wordcraft",
        "WordCraft",
        "Writing and document design",
        "Microsoft Word",
        AppGroup::New,
        [[0x3f, 0x5f, 0xe0], [0x2a, 0x44, 0xb8], [0x8e, 0xa2, 0xff]],
        cinch_repo!("wordcraft"),
    ),
    app(
        "deckcraft",
        "DeckCraft",
        "Presentations and slide shows",
        "Microsoft PowerPoint",
        AppGroup::New,
        [[0xf0, 0x7a, 0x1e], [0xa8, 0x4f, 0x0a], [0xff, 0xa0, 0x60]],
        cinch_repo!("deckcraft"),
    ),
    app(
        "gridcraft",
        "GridCraft",
        "Spreadsheets and data",
        "Microsoft Excel",
        AppGroup::New,
        [[0x1f, 0x9d, 0x55], [0x12, 0x70, 0x3a], [0x4f, 0xd6, 0x8a]],
        cinch_repo!("gridcraft"),
    ),
    app(
        "cadcraft",
        "CADCraft",
        "Computer-aided design and drafting",
        "Autodesk AutoCAD",
        AppGroup::New,
        [[0x3e, 0xa6, 0xe8], [0x1a, 0x6f, 0xa8], [0x7c, 0xc8, 0xff]],
        cinch_repo!("cadcraft"),
    ),
    app(
        "soundcraft",
        "SoundCraft",
        "Recording, editing and mixing audio",
        "Avid Pro Tools",
        AppGroup::New,
        [[0x1f, 0xa5, 0xc9], [0x0e, 0x72, 0x90], [0x5f, 0xd0, 0xee]],
        cinch_repo!("soundcraft"),
    ),
];

/// Shown wherever the products the apps are inspired by are named.
pub const TRADEMARK_NOTICE: &str = "Adobe, Photoshop, Illustrator, Premiere Pro, Lightroom, Acrobat, After Effects and InDesign are either \
registered trademarks or trademarks of Adobe in the United States and/or other countries. Microsoft, Word, PowerPoint and Excel are \
trademarks of the Microsoft group of companies. Autodesk and AutoCAD are registered trademarks or trademarks of Autodesk, Inc. Avid \
and Pro Tools are trademarks or registered trademarks of Avid Technology, Inc. All other trademarks are the property of their \
respective owners. These names are used only to describe what each Craft App is for; the Craft App Suite is independent and is not \
affiliated with, sponsored by or endorsed by any of these companies.";

/// The Cinch forks on GitHub: every Craft App's releases.
pub const GITHUB_URL: &str = concat!("https://github.com/", cinch_owner!());

/// ArtCraft Launcher itself, described like a Craft App (asset names `artcraft-launcher-<ver>-…`,
/// bundle id `ai.storyteller.artcraft-launcher`, `ArtCraft Launcher.app`). Not part of [`SUITE`].
/// Self-update is off in this build, so its repo is never asked about.
pub const LAUNCHER: CraftApp = app(
    "artcraft-launcher",
    "ArtCraft Launcher",
    "Installs and updates the Craft App Suite",
    "",
    AppGroup::New,
    [[0x0b, 0x0b, 0x0c], [0x0b, 0x0b, 0x0c], [0xf2, 0xf1, 0xee]],
    cinch_repo!("craft-launcher"),
);

/// The launcher's own asset stem.
pub const LAUNCHER_ID: &str = "artcraft-launcher";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suite_has_seven_classic_and_five_new_apps_with_unique_ids() {
        assert_eq!(SUITE.iter().filter(|a| a.group == AppGroup::Classic).count(), 7);
        assert_eq!(SUITE.iter().filter(|a| a.group == AppGroup::New).count(), 5);
        let mut ids: Vec<_> = SUITE.iter().map(|a| a.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), SUITE.len());
        for a in &SUITE {
            assert_eq!(a.repo, format!("{CINCH_OWNER}/{}", a.id));
            assert_eq!(a.name.to_lowercase(), a.id);
        }
        assert_eq!(LAUNCHER.repo, format!("{CINCH_OWNER}/craft-launcher"));
        assert_eq!(GITHUB_URL, format!("https://github.com/{CINCH_OWNER}"));
    }

    /// packaging/windows/package.ps1 reads the owner out of this file's `cinch_owner!` macro for
    /// the MSI's links; keep the macro in the shape its pattern expects.
    #[test]
    fn the_windows_packaging_can_read_the_owner() {
        let source = include_str!("catalog.rs");
        let shape = format!("macro_rules! cinch_owner {{\n    () => {{\n        \"{CINCH_OWNER}\"");
        assert!(source.contains(&shape), "package.ps1 can't find the owner");
        assert!(CINCH_OWNER.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
    }

    #[test]
    fn every_inspiring_product_is_covered_by_the_trademark_notice() {
        for a in &SUITE {
            assert!(a.inspired_by().ends_with('®'));
            // The company and the product name both appear in the notice.
            let (company, product) = a.like.split_once(' ').unwrap();
            assert!(TRADEMARK_NOTICE.contains(company), "{company}");
            assert!(TRADEMARK_NOTICE.contains(product), "{product}");
        }
    }

    #[test]
    fn pdfcraft_also_answers_to_printcraft() {
        let pdf = CraftApp::by_id("pdfcraft").unwrap();
        assert_eq!(pdf.stems().collect::<Vec<_>>(), ["pdfcraft", "printcraft"]);
        assert_eq!(pdf.website().as_deref(), Some("https://getartcraft.com/apps/pdfcraft"));
        assert_eq!(CraftApp::by_id("wordcraft").unwrap().website(), None);
    }
}
