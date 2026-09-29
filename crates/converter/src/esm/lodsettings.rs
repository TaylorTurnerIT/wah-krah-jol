//! Readers for Skyrim's `lodsettings/<WorldspaceEDID>.lod` sidecars.
//!
//! Each file is exactly 16 bytes: four little-endian `i32` values
//! `[X, Y, Width, Height]` (`xEdit wbLOD.pas`). `(X, Y)` is the southwest
//! corner of the LOD grid in cell units, which is the per-worldspace origin
//! every LOD tier anchors to (GEOM-02). `Width`/`Height` bound the compiled
//! area. A file that is missing, truncated, or overlong is not an origin:
//! the caller skips that world's LOD with an actionable error rather than
//! assuming zero.

use color_eyre::{Result, eyre::WrapErr};
use shared::lod::LodOrigin;
use std::path::Path;

/// A parsed `.lod` sidecar: the LOD grid origin plus the compiled extent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LodSettings {
    pub origin: LodOrigin,
    pub width: i32,
    pub height: i32,
}

impl LodSettings {
    /// Reads and validates one sidecar file. The path identifies the file in
    /// error messages, so a skip-the-world error can name it.
    pub fn read(path: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
        Self::parse(&bytes).wrap_err_with(|| format!("invalid LOD settings {}", path.display()))
    }

    fn parse(bytes: &[u8]) -> Result<Self> {
        color_eyre::eyre::ensure!(
            bytes.len() == 16,
            "expected a 16-byte [X, Y, Width, Height] file, found {} bytes",
            bytes.len()
        );
        let field =
            |offset: usize| i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        let (grid_x, grid_y, width, height) = (field(0), field(4), field(8), field(12));
        color_eyre::eyre::ensure!(
            width > 0 && height > 0,
            "LOD extent must be positive, found {width}x{height}"
        );
        Ok(Self {
            origin: LodOrigin::new(grid_x, grid_y),
            width,
            height,
        })
    }
}

/// The sidecar path for a worldspace editor id inside a Skyrim data layout:
/// `lodsettings/<WorldspaceEDID>.lod`. Matching is exact: Bethesda ships one
/// file per worldspace named for its editor id.
///
/// The id is untrusted input at a trust boundary: it must be exactly one
/// ordinary filename component. Empty ids, absolute paths, `.`/`..`, and any
/// id containing `/`, `\`, or `:` is rejected rather than joined.
pub fn sidecar_path(data_dir: &Path, worldspace_editor_id: &str) -> Result<std::path::PathBuf> {
    color_eyre::eyre::ensure!(
        !worldspace_editor_id.is_empty(),
        "worldspace editor id must not be empty"
    );
    color_eyre::eyre::ensure!(
        !worldspace_editor_id.contains('/')
            && !worldspace_editor_id.contains('\\')
            && !worldspace_editor_id.contains(':'),
        "worldspace editor id {worldspace_editor_id:?} must not contain `/`, `\\`, or `:`"
    );
    color_eyre::eyre::ensure!(
        worldspace_editor_id != "." && worldspace_editor_id != "..",
        "worldspace editor id {worldspace_editor_id:?} must not be `.` or `..`"
    );
    color_eyre::eyre::ensure!(
        !Path::new(worldspace_editor_id).is_absolute(),
        "worldspace editor id {worldspace_editor_id:?} must not be an absolute path"
    );
    {
        let mut components = Path::new(worldspace_editor_id).components();
        let is_single_normal = matches!(components.next(), Some(std::path::Component::Normal(_)))
            && components.next().is_none();
        color_eyre::eyre::ensure!(
            is_single_normal,
            "worldspace editor id {worldspace_editor_id:?} must be exactly one ordinary filename component"
        );
    }
    Ok(data_dir
        .join("lodsettings")
        .join(format!("{worldspace_editor_id}.lod")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_bytes(x: i32, y: i32, width: i32, height: i32) -> Vec<u8> {
        [x, y, width, height]
            .into_iter()
            .flat_map(i32::to_le_bytes)
            .collect()
    }

    #[test]
    fn parses_origin_and_extent() {
        let settings = LodSettings::parse(&settings_bytes(-64, -48, 128, 96)).unwrap();
        assert_eq!(settings.origin, LodOrigin::new(-64, -48));
        assert_eq!((settings.width, settings.height), (128, 96));
    }

    #[test]
    fn rejects_wrong_lengths() {
        for len in [0, 4, 12, 15, 17, 32] {
            assert!(
                LodSettings::parse(&vec![0u8; len]).is_err(),
                "{len} bytes must not parse"
            );
        }
    }

    #[test]
    fn rejects_nonpositive_extent() {
        for (width, height) in [(0, 96), (128, 0), (-1, 96), (128, -4)] {
            assert!(
                LodSettings::parse(&settings_bytes(0, 0, width, height)).is_err(),
                "{width}x{height} must not parse"
            );
        }
    }

    #[test]
    fn sidecar_path_uses_the_worldspace_editor_id() {
        assert_eq!(
            sidecar_path(Path::new("/data"), "Tamriel").unwrap(),
            Path::new("/data/lodsettings/Tamriel.lod")
        );
    }

    #[test]
    fn sidecar_path_accepts_ordinary_editor_ids() {
        for id in [
            "Tamriel",
            "WhiterunWorld",
            "DLC01Hearthfire",
            "My_World01",
            "a.b",
        ] {
            assert!(
                sidecar_path(Path::new("/data"), id).is_ok(),
                "{id:?} must be accepted"
            );
        }
    }

    #[test]
    fn sidecar_path_rejects_untrusted_editor_ids() {
        for id in [
            "",
            ".",
            "..",
            "Tamriel/evil",
            "../Tamriel",
            "/Tamriel",
            "/etc/passwd",
            "Tamriel\\evil",
            "..\\Tamriel",
            "C:\\Tamriel",
            "C:Tamriel",
            "a:b",
            "a/b",
        ] {
            assert!(
                sidecar_path(Path::new("/data"), id).is_err(),
                "{id:?} must be rejected"
            );
        }
    }
}
